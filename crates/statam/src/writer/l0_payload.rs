//! Local L0 payload frame builder stored inline inside each Row Group.

use crate::StatamError;
use crate::format::{
    BLOB_DEDICATED_FRAME_BYTES, BLOB_MAX_CHUNK_BYTES, BLOB_SMALL_FRAME_TARGET_BYTES, ChunkLoc,
    Codec, MAX_FRAME_RAW_BYTES, MAX_ROW_PAYLOAD_BYTES, write_framed_pack_frame_with_min_savings,
};
use crate::parallel::{MAX_PARALLEL_THREADS, parallel_map_in_order_static};

use super::default_l1_codec;

/// Target uncompressed bytes per L0 local payload `PF` frame (512 KiB).
const L0_PAYLOAD_FRAME_TARGET_BYTES: usize = 512 * 1024;
/// Minimum compression savings required for binary blob frames before falling back to `Codec::None`.
const BLOB_MIN_SAVINGS_PCT: u8 = 12;
/// Maximum total raw bytes queued in `LocalL0PayloadBuilder::pending_frames` before flushing (32 MiB).
const MAX_PENDING_RAW_BYTES: usize = 32 * 1024 * 1024;
/// Magic bytes for the local L0 payload directory inside `l0_payload_offset`.
pub(crate) const L0_PAYLOAD_DIR_MAGIC: [u8; 4] = *b"L0P1";
/// Pack index sentinel indicating local L0 payload frames inside the Row Group.
pub const L0_LOCAL_PACK_IDX: u16 = 0xFFFF;

type PendingL0Frame = (Vec<u8>, Vec<(u32, u32)>, u8);

pub(super) struct LocalL0PayloadBuilder {
    zstd_level: i32,
    write_threads: usize,
    frames: Vec<(u64, u32, u32, Vec<u8>)>, // (rel_frame_off, stored_len, raw_len, framed_bytes)
    compressed_frames_bytes: usize,
    chunks: Vec<ChunkLoc>,
    cur_raw: Vec<u8>,
    pub(super) cur_chunks: Vec<(u32, u32)>, // (offset_in_frame, raw_len)
    cur_min_savings_pct: u8,
    pub(super) pending_frames: Vec<PendingL0Frame>,
    pending_chunk_count: usize,
    pending_frame_raw_bytes: usize,
}

impl LocalL0PayloadBuilder {
    pub(super) fn new(zstd_level: i32) -> Self {
        Self::new_with_threads(zstd_level, 1)
    }

    pub(super) fn new_with_threads(zstd_level: i32, write_threads: usize) -> Self {
        Self {
            zstd_level,
            write_threads,
            frames: Vec::new(),
            compressed_frames_bytes: 0,
            chunks: Vec::new(),
            cur_raw: Vec::new(),
            cur_chunks: Vec::new(),
            cur_min_savings_pct: 0,
            pending_frames: Vec::new(),
            pending_chunk_count: 0,
            pending_frame_raw_bytes: 0,
        }
    }

    pub(super) fn estimated_stored_bytes(&self) -> usize {
        if self.frames.is_empty() && self.pending_frames.is_empty() && self.cur_chunks.is_empty() {
            return 0;
        }
        let total_frames = self
            .frames
            .len()
            .saturating_add(self.pending_frames.len())
            .saturating_add(usize::from(!self.cur_chunks.is_empty()));
        let total_chunks = self
            .chunks
            .len()
            .saturating_add(self.pending_chunk_count)
            .saturating_add(self.cur_chunks.len());
        12usize
            .saturating_add(total_frames.saturating_mul(16))
            .saturating_add(total_chunks.saturating_mul(12))
            .saturating_add(self.compressed_frames_bytes)
            .saturating_add(self.pending_frame_raw_bytes)
            .saturating_add(self.cur_raw.len())
    }

    pub(super) fn add_chunk(&mut self, bytes: &[u8]) -> Result<u32, StatamError> {
        if bytes.len() > MAX_FRAME_RAW_BYTES {
            return Err(StatamError::BoundsExceeded {
                reason: "L0 payload chunk exceeds MAX_FRAME_RAW_BYTES",
            });
        }
        if self.cur_min_savings_pct != 0 && !self.cur_chunks.is_empty() {
            self.flush_frame()?;
        }
        self.cur_min_savings_pct = 0;
        if !self.cur_raw.is_empty()
            && self.cur_raw.len().saturating_add(bytes.len()) > L0_PAYLOAD_FRAME_TARGET_BYTES
        {
            self.flush_frame()?;
        }
        let cid =
            u32::try_from(self.chunks.len() + self.pending_chunk_count + self.cur_chunks.len())
                .map_err(|_| StatamError::BoundsExceeded {
                    reason: "L0 payload chunk_id overflows u32",
                })?;
        let off_in_frame =
            u32::try_from(self.cur_raw.len()).map_err(|_| StatamError::BoundsExceeded {
                reason: "L0 payload offset_in_frame overflows u32",
            })?;
        let raw_len = u32::try_from(bytes.len()).map_err(|_| StatamError::BoundsExceeded {
            reason: "L0 payload chunk raw_len overflows u32",
        })?;
        self.cur_raw.extend_from_slice(bytes);
        self.cur_chunks.push((off_in_frame, raw_len));

        if self.cur_raw.len() >= L0_PAYLOAD_FRAME_TARGET_BYTES {
            self.flush_frame()?;
        }
        Ok(cid)
    }

    pub(super) fn add_blob(&mut self, bytes: &[u8]) -> Result<Vec<u32>, StatamError> {
        let mut out = Vec::new();
        self.add_blob_into(bytes, &mut out)?;
        Ok(out)
    }

    pub(super) fn add_blob_into(
        &mut self,
        bytes: &[u8],
        out_cids: &mut Vec<u32>,
    ) -> Result<(), StatamError> {
        if bytes.len() > MAX_ROW_PAYLOAD_BYTES {
            return Err(StatamError::BoundsExceeded {
                reason: "blob payload exceeds MAX_ROW_PAYLOAD_BYTES",
            });
        }
        if bytes.len() < BLOB_DEDICATED_FRAME_BYTES {
            if self.cur_min_savings_pct != BLOB_MIN_SAVINGS_PCT && !self.cur_chunks.is_empty() {
                self.flush_frame()?;
            }
            self.cur_min_savings_pct = BLOB_MIN_SAVINGS_PCT;
            if !self.cur_raw.is_empty()
                && self.cur_raw.len().saturating_add(bytes.len()) > BLOB_SMALL_FRAME_TARGET_BYTES
            {
                self.flush_frame()?;
            }
            let cid =
                u32::try_from(self.chunks.len() + self.pending_chunk_count + self.cur_chunks.len())
                    .map_err(|_| StatamError::BoundsExceeded {
                        reason: "L0 payload chunk_id overflows u32",
                    })?;
            let off_in_frame =
                u32::try_from(self.cur_raw.len()).map_err(|_| StatamError::BoundsExceeded {
                    reason: "L0 payload offset_in_frame overflows u32",
                })?;
            let raw_len = u32::try_from(bytes.len()).map_err(|_| StatamError::BoundsExceeded {
                reason: "L0 payload chunk raw_len overflows u32",
            })?;
            self.cur_raw.extend_from_slice(bytes);
            self.cur_chunks.push((off_in_frame, raw_len));
            if self.cur_raw.len() >= BLOB_SMALL_FRAME_TARGET_BYTES {
                self.flush_frame()?;
            }
            out_cids.push(cid);
            return Ok(());
        }

        if !self.cur_chunks.is_empty() {
            self.flush_frame()?;
        }
        self.cur_min_savings_pct = BLOB_MIN_SAVINGS_PCT;
        for slice in bytes.chunks(BLOB_MAX_CHUNK_BYTES) {
            let cid =
                u32::try_from(self.chunks.len() + self.pending_chunk_count + self.cur_chunks.len())
                    .map_err(|_| StatamError::BoundsExceeded {
                        reason: "L0 payload chunk_id overflows u32",
                    })?;
            let raw_len = u32::try_from(slice.len()).map_err(|_| StatamError::BoundsExceeded {
                reason: "L0 payload chunk raw_len overflows u32",
            })?;
            self.cur_raw.extend_from_slice(slice);
            self.cur_chunks.push((0, raw_len));
            self.flush_frame()?;
            out_cids.push(cid);
        }
        Ok(())
    }

    pub(super) fn flush_frame(&mut self) -> Result<(), StatamError> {
        if self.cur_chunks.is_empty() {
            return Ok(());
        }
        let min_savings_pct = self.cur_min_savings_pct;
        if self.write_threads > 1 {
            let batch_cap = self.write_threads.clamp(1, MAX_PARALLEL_THREADS);
            if !self.pending_frames.is_empty()
                && (self.pending_frames.len() >= batch_cap
                    || self
                        .pending_frame_raw_bytes
                        .saturating_add(self.cur_raw.len())
                        > MAX_PENDING_RAW_BYTES)
            {
                self.flush_pending()?;
            }
            let raw = std::mem::take(&mut self.cur_raw);
            let frame_chunks = std::mem::take(&mut self.cur_chunks);
            self.pending_chunk_count = self.pending_chunk_count.saturating_add(frame_chunks.len());
            self.pending_frame_raw_bytes = self.pending_frame_raw_bytes.saturating_add(raw.len());
            self.pending_frames
                .push((raw, frame_chunks, min_savings_pct));
            if self.pending_frames.len() >= batch_cap
                || self.pending_frame_raw_bytes >= MAX_PENDING_RAW_BYTES
            {
                self.flush_pending()?;
            }
            return Ok(());
        }
        let mut framed = Vec::new();
        let codec = if self.cur_raw.len() < 1024 {
            Codec::Lz4
        } else {
            default_l1_codec()
        };
        let info = write_framed_pack_frame_with_min_savings(
            &self.cur_raw,
            codec,
            0,
            self.zstd_level,
            min_savings_pct,
            &mut framed,
        )?;
        let fid = u32::try_from(self.frames.len()).map_err(|_| StatamError::BoundsExceeded {
            reason: "L0 payload frame_id overflows u32",
        })?;
        for (off_in_frame, raw_len) in self.cur_chunks.drain(..) {
            self.chunks.push(ChunkLoc {
                frame_id: fid,
                offset_in_frame: off_in_frame,
                raw_len,
            });
        }
        self.compressed_frames_bytes = self.compressed_frames_bytes.saturating_add(framed.len());
        self.frames.push((0, info.stored_len, info.raw_len, framed));
        self.cur_raw.clear();
        Ok(())
    }

    pub(super) fn flush_pending(&mut self) -> Result<(), StatamError> {
        let batch = std::mem::take(&mut self.pending_frames);
        self.pending_chunk_count = 0;
        self.pending_frame_raw_bytes = 0;
        if batch.is_empty() {
            return Ok(());
        }
        let zstd_level = self.zstd_level;
        let encoded = parallel_map_in_order_static(
            batch,
            self.write_threads,
            move |(raw, frame_chunks, min_savings_pct)| {
                let mut framed = Vec::new();
                let codec = if raw.len() < 1024 {
                    Codec::Lz4
                } else {
                    default_l1_codec()
                };
                let info = write_framed_pack_frame_with_min_savings(
                    &raw,
                    codec,
                    0,
                    zstd_level,
                    min_savings_pct,
                    &mut framed,
                )?;
                drop(raw);
                Ok((info.stored_len, info.raw_len, framed, frame_chunks))
            },
        )?;
        for (stored_len, raw_len, framed, frame_chunks) in encoded {
            let fid =
                u32::try_from(self.frames.len()).map_err(|_| StatamError::BoundsExceeded {
                    reason: "L0 payload frame_id overflows u32",
                })?;
            for (off_in_frame, chunk_raw_len) in frame_chunks {
                self.chunks.push(ChunkLoc {
                    frame_id: fid,
                    offset_in_frame: off_in_frame,
                    raw_len: chunk_raw_len,
                });
            }
            self.compressed_frames_bytes =
                self.compressed_frames_bytes.saturating_add(framed.len());
            self.frames.push((0, stored_len, raw_len, framed));
        }
        Ok(())
    }

    pub(super) fn finish(mut self) -> Result<Vec<u8>, StatamError> {
        if !self.cur_chunks.is_empty() {
            self.flush_frame()?;
        }
        if !self.pending_frames.is_empty() {
            self.flush_pending()?;
        }
        if self.chunks.is_empty() {
            return Ok(Vec::new());
        }

        let header_len = 12 + self.frames.len() * 16 + self.chunks.len() * 12;
        let mut rel_off = header_len as u64;
        for (off, _, _, framed) in &mut self.frames {
            *off = rel_off;
            rel_off =
                rel_off
                    .checked_add(framed.len() as u64)
                    .ok_or(StatamError::BoundsExceeded {
                        reason: "L0 payload region size overflow",
                    })?;
        }
        let total_len = usize::try_from(rel_off).map_err(|_| StatamError::BoundsExceeded {
            reason: "L0 payload region overflows usize",
        })?;
        let mut out = Vec::with_capacity(total_len);
        out.extend_from_slice(&L0_PAYLOAD_DIR_MAGIC);
        out.extend_from_slice(&(self.frames.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.chunks.len() as u32).to_le_bytes());
        for (off, stored_len, raw_len, _) in &self.frames {
            out.extend_from_slice(&off.to_le_bytes());
            out.extend_from_slice(&stored_len.to_le_bytes());
            out.extend_from_slice(&raw_len.to_le_bytes());
        }
        for c in &self.chunks {
            out.extend_from_slice(&c.encode());
        }
        for (_, _, _, framed) in &self.frames {
            out.extend_from_slice(framed);
        }
        Ok(out)
    }
}
