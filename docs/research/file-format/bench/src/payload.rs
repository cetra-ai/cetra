//! Prototype of a payload layer for large, repetitive strings, measured
//! against page-style zstd. Single-threaded, per column chunk (one file's
//! column), so numbers compare per-core cost.
//!
//! Schemes:
//!   page1m    values concatenated into ~1 MiB raw pages, zstd-3 each (Parquet-like)
//!   block64k  ~64 KiB raw blocks, zstd-3 (random-access oriented)
//!   cdc       content-defined chunks (gear hash, avg ~2 KiB), deduplicated per
//!             column chunk; unique chunks packed in ~128 KiB raw zstd-3 frames;
//!             rows are lists of chunk ids; per-frame trigram bloom filter.

use std::collections::HashMap;
use std::time::Instant;

use memchr::memmem;

pub struct Column {
    pub name: String,
    pub values: Vec<Vec<u8>>, // null = empty
}

#[derive(Default, Clone)]
pub struct Stats {
    pub raw: u64,
    pub stored: u64,
    pub encode_s: f64,
    pub decode_s: f64,
    pub lookup_bytes: u64,
    pub lookup_s: f64,
    pub lookups: u64,
    pub search_s: f64,
    pub search_hits: u64,
    pub search_frames_read: u64,
    pub frames: u64,
}

impl Stats {
    fn add(&mut self, o: &Stats) {
        self.raw += o.raw;
        self.stored += o.stored;
        self.encode_s += o.encode_s;
        self.decode_s += o.decode_s;
        self.lookup_bytes += o.lookup_bytes;
        self.lookup_s += o.lookup_s;
        self.lookups += o.lookups;
        self.search_s += o.search_s;
        self.search_hits += o.search_hits;
        self.search_frames_read += o.search_frames_read;
        self.frames += o.frames;
    }
}

// ------------------------------------------------------------ blocked zstd

struct Blocked {
    frames: Vec<Vec<u8>>,
    raw_len: Vec<usize>,
    dict: Option<Vec<u8>>,
    /// first row of each frame; rows never span frames
    first_row: Vec<usize>,
    /// per row: (frame, offset, len) in the decompressed frame
    rows: Vec<(u32, u32, u32)>,
}

fn blocked(values: &[Vec<u8>], target: usize, dict: Option<Vec<u8>>) -> Blocked {
    let mut comp = match &dict {
        Some(d) => zstd::bulk::Compressor::with_dictionary(3, d).unwrap(),
        None => zstd::bulk::Compressor::new(3).unwrap(),
    };
    let mut raw_len = Vec::new();
    let mut frames = Vec::new();
    let mut first_row = Vec::new();
    let mut rows = Vec::with_capacity(values.len());
    let mut buf = Vec::new();
    let mut start = 0;
    for (i, v) in values.iter().enumerate() {
        if !buf.is_empty() && buf.len() + v.len() > target {
            frames.push(comp.compress(&buf).unwrap());
            raw_len.push(buf.len());
            first_row.push(start);
            buf.clear();
            start = i;
        }
        rows.push((frames.len() as u32, buf.len() as u32, v.len() as u32));
        buf.extend_from_slice(v);
    }
    if !buf.is_empty() || frames.is_empty() {
        frames.push(comp.compress(&buf).unwrap());
        raw_len.push(buf.len());
        first_row.push(start);
    }
    Blocked { frames, raw_len, dict, first_row, rows }
}

fn blocked_stats(col: &Column, target: usize, picks: &[usize], needle: &[u8], train: bool) -> Stats {
    let raw: u64 = col.values.iter().map(|v| v.len() as u64).sum();
    let t = Instant::now();
    let dict = train.then(|| {
        let samples: Vec<&[u8]> = col.values.iter().step_by((col.values.len() / 4000).max(1)).filter(|v| !v.is_empty()).map(|v| &v[..v.len().min(8192)]).collect();
        zstd::dict::from_samples(&samples, 112_640).unwrap_or_default()
    });
    let b = blocked(&col.values, target, dict);
    let mut dec = match &b.dict {
        Some(d) => zstd::bulk::Decompressor::with_dictionary(d).unwrap(),
        None => zstd::bulk::Decompressor::new().unwrap(),
    };
    let encode_s = t.elapsed().as_secs_f64();
    // offsets: 4 bytes per row (lengths), uncompressed upper bound
    let stored = b.frames.iter().map(|f| f.len() as u64).sum::<u64>() + col.values.len() as u64 * 4 + b.dict.as_ref().map_or(0, |d| d.len() as u64);
    let t = Instant::now();
    let mut total = 0usize;
    for (i, f) in b.frames.iter().enumerate() {
        let d = dec.decompress(f, b.raw_len[i]).unwrap();
        total += d.len();
    }
    std::hint::black_box(total);
    let decode_s = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let mut lookup_bytes = 0;
    for &r in picks {
        let (fi, off, len) = b.rows[r];
        let f = &b.frames[fi as usize];
        lookup_bytes += f.len() as u64;
        let d = dec.decompress(f, b.raw_len[fi as usize]).unwrap();
        std::hint::black_box(&d[off as usize..(off + len) as usize]);
    }
    let lookup_s = t.elapsed().as_secs_f64();
    // substring search: decompress everything, scan each row
    let t = Instant::now();
    let finder = memmem::Finder::new(needle);
    let mut hits = 0;
    for (fi, f) in b.frames.iter().enumerate() {
        let d = dec.decompress(f, b.raw_len[fi]).unwrap();
        let end = b.first_row.get(fi + 1).copied().unwrap_or(b.rows.len());
        for r in b.first_row[fi]..end {
            let (_, off, len) = b.rows[r];
            if finder.find(&d[off as usize..(off + len) as usize]).is_some() {
                hits += 1;
            }
        }
    }
    let search_s = t.elapsed().as_secs_f64();
    Stats {
        raw,
        stored,
        encode_s,
        decode_s,
        lookup_bytes,
        lookup_s,
        lookups: picks.len() as u64,
        search_s,
        search_hits: hits,
        search_frames_read: b.frames.len() as u64,
        frames: b.frames.len() as u64,
    }
}

// ------------------------------------------------------------ CDC dedup

struct Gear([u64; 256]);
impl Gear {
    fn new() -> Self {
        let mut t = [0u64; 256];
        let mut x: u64 = 0x1234_5678_9abc_def1;
        for v in &mut t {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *v = x;
        }
        Self(t)
    }
    /// Cut points: min 512, avg ~2 KiB (mask 11 bits past min), max 8 KiB.
    fn chunks<'a>(&self, data: &'a [u8], out: &mut Vec<&'a [u8]>) {
        const MIN: usize = 512;
        const MAX: usize = 8192;
        const MASK: u64 = (1 << 11) - 1;
        let mut start = 0;
        while start < data.len() {
            let rest = &data[start..];
            if rest.len() <= MIN {
                out.push(rest);
                break;
            }
            let mut h: u64 = 0;
            let mut cut = rest.len().min(MAX);
            for (i, &b) in rest.iter().enumerate().take(MAX).skip(MIN) {
                h = (h << 1).wrapping_add(self.0[b as usize]);
                if h >> 52 & MASK == 0 {
                    cut = i + 1;
                    break;
                }
            }
            out.push(&rest[..cut]);
            start += cut;
        }
    }
}


struct Cdc {
    bb: usize,
    frames: Vec<Vec<u8>>,
    raw_len: Vec<usize>,
    /// per unique chunk: (frame, offset, len)
    chunks: Vec<(u32, u32, u32)>,
    /// per row: chunk ids
    refs: Vec<Vec<u32>>,
    blooms: Vec<Vec<u64>>,
    /// first and last EDGE bytes of each unique chunk, for needles that
    /// cross a chunk boundary (needles up to EDGE+1 bytes)
    edges: Vec<u8>,
}

const EDGE: usize = 63;

fn tri_hash(t: &[u8]) -> usize {
    let x = (t[0] as u64) | (t[1] as u64) << 8 | (t[2] as u64) << 16;
    (x.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 40) as usize
}

fn cdc(values: &[Vec<u8>], frame_target: usize, level: i32, bb: usize) -> Cdc {
    let gear = Gear::new();
    let mut index: HashMap<u128, u32> = HashMap::new();
    let mut frames = Vec::new();
    let mut blooms = Vec::new();
    let mut chunks = Vec::new();
    let mut raw_len = Vec::new();
    let mut edges = Vec::new();
    let mut refs = Vec::with_capacity(values.len());
    let mut buf: Vec<u8> = Vec::new();
    let mut bloom = vec![0u64; bb / 64];
    let mut pieces = Vec::new();
    for v in values {
        pieces.clear();
        gear.chunks(v, &mut pieces);
        let mut row = Vec::with_capacity(pieces.len());
        for p in &pieces {
            let h = xxhash_rust::xxh3::xxh3_128(p);
            let id = *index.entry(h).or_insert_with(|| {
                if !buf.is_empty() && buf.len() + p.len() > frame_target {
                    frames.push(zstd::bulk::compress(&buf, level).unwrap());
                    raw_len.push(buf.len());
                    blooms.push(std::mem::replace(&mut bloom, vec![0u64; bb / 64]));
                    buf.clear();
                }
                for w in p.windows(3) {
                    let b = tri_hash(w) % bb;
                    bloom[b / 64] |= 1 << (b % 64);
                }
                chunks.push((frames.len() as u32, buf.len() as u32, p.len() as u32));
                let e = EDGE.min(p.len());
                edges.extend_from_slice(&p[..e]);
                edges.extend_from_slice(&p[p.len() - e..]);
                buf.extend_from_slice(p);
                (chunks.len() - 1) as u32
            });
            row.push(id);
        }
        refs.push(row);
    }
    if !buf.is_empty() {
        frames.push(zstd::bulk::compress(&buf, level).unwrap());
        raw_len.push(buf.len());
        blooms.push(bloom);
    }
    Cdc { bb, frames, raw_len, chunks, refs, blooms, edges }
}

fn varint_len(mut x: u64) -> u64 {
    let mut n = 1;
    while x >= 0x80 {
        x >>= 7;
        n += 1;
    }
    n
}

fn cdc_stats(col: &Column, picks: &[usize], needle: &[u8], level: i32, with_bloom: bool, frame: usize, bb: usize) -> Stats {
    let raw: u64 = col.values.iter().map(|v| v.len() as u64).sum();
    let t = Instant::now();
    let c = cdc(&col.values, frame, level, bb);
    // Row refs as delta-from-previous-id varints, zstd'd (counted in encode time).
    let mut refbuf = Vec::new();
    for r in &c.refs {
        let mut x = r.len() as u64;
        loop {
            let b = (x & 0x7f) as u8;
            x >>= 7;
            if x == 0 {
                refbuf.push(b);
                break;
            }
            refbuf.push(b | 0x80);
        }
        let mut prev = 0i64;
        for &id in r {
            let d = id as i64 - prev;
            prev = id as i64;
            let mut z = ((d << 1) ^ (d >> 63)) as u64;
            loop {
                let b = (z & 0x7f) as u8;
                z >>= 7;
                if z == 0 {
                    refbuf.push(b);
                    break;
                }
                refbuf.push(b | 0x80);
            }
        }
    }
    let refs_z = zstd::bulk::compress(&refbuf, 3).unwrap();
    let edges_z = zstd::bulk::compress(&c.edges, 3).unwrap();
    let encode_s = t.elapsed().as_secs_f64();
    let chunk_table: u64 = c.chunks.iter().map(|&(_, o, l)| varint_len(o as u64) + varint_len(l as u64)).sum();
    let bloom_bytes = if with_bloom { c.blooms.len() as u64 * c.bb as u64 / 8 } else { 0 };
    let stored = c.frames.iter().map(|f| f.len() as u64).sum::<u64>() + refs_z.len() as u64 + edges_z.len() as u64 + chunk_table + bloom_bytes;

    // Full decode: every frame once, then rebuild every row.
    let t = Instant::now();
    let dec: Vec<Vec<u8>> = c.frames.iter().enumerate().map(|(i, f)| zstd::bulk::decompress(f, c.raw_len[i]).unwrap()).collect();
    let mut total = 0usize;
    let mut row = Vec::new();
    for r in &c.refs {
        row.clear();
        for &id in r {
            let (fi, o, l) = c.chunks[id as usize];
            row.extend_from_slice(&dec[fi as usize][o as usize..(o + l) as usize]);
        }
        total += row.len();
    }
    std::hint::black_box(total);
    let decode_s = t.elapsed().as_secs_f64();
    drop(dec);

    // Point lookup: decompress only the frames a row references.
    let t = Instant::now();
    let mut lookup_bytes = 0;
    for &r in picks {
        let mut need: Vec<u32> = c.refs[r].iter().map(|&id| c.chunks[id as usize].0).collect();
        need.sort_unstable();
        need.dedup();
        let mut cache = HashMap::new();
        for fi in need {
            lookup_bytes += c.frames[fi as usize].len() as u64;
            cache.insert(fi, zstd::bulk::decompress(&c.frames[fi as usize], c.raw_len[fi as usize]).unwrap());
        }
        let mut out = Vec::new();
        for &id in &c.refs[r] {
            let (fi, o, l) = c.chunks[id as usize];
            out.extend_from_slice(&cache[&fi][o as usize..(o + l) as usize]);
        }
        std::hint::black_box(out);
    }
    let lookup_s = t.elapsed().as_secs_f64();

    // Substring search on unique chunks only (+ chunk-boundary pairs),
    // skipping frames whose trigram bloom excludes the needle.
    let t = Instant::now();
    let finder = memmem::Finder::new(needle);
    let tris: Vec<usize> = needle.windows(3).map(|w| tri_hash(w) % c.bb).collect();
    let mut frames_read = 0;
    let mut chunk_hit = vec![false; c.chunks.len()];
    let mut dec_cache: HashMap<u32, Vec<u8>> = HashMap::new();
    let frame_may = |fi: usize| !with_bloom || tris.iter().all(|&b| c.blooms[fi][b / 64] >> (b % 64) & 1 == 1);
    let mut first_chunk_of_frame = vec![usize::MAX; c.frames.len()];
    for (id, &(fi, _, _)) in c.chunks.iter().enumerate() {
        if first_chunk_of_frame[fi as usize] == usize::MAX {
            first_chunk_of_frame[fi as usize] = id;
        }
    }
    for fi in 0..c.frames.len() {
        if !frame_may(fi) {
            continue;
        }
        frames_read += 1;
        let d = zstd::bulk::decompress(&c.frames[fi], c.raw_len[fi]).unwrap();
        let mut id = first_chunk_of_frame[fi];
        while id < c.chunks.len() && c.chunks[id].0 as usize == fi {
            let (_, o, l) = c.chunks[id];
            if finder.find(&d[o as usize..(o + l) as usize]).is_some() {
                chunk_hit[id] = true;
            }
            id += 1;
        }
        dec_cache.insert(fi as u32, d);
    }
    // Needles crossing a chunk boundary: check each distinct adjacent pair's
    // boundary window from the edge table (no frame decompression).
    assert!(needle.len() <= EDGE + 1, "boundary check supports needles up to EDGE+1 bytes");
    let edges = zstd::bulk::decompress(&edges_z, 1 << 30).unwrap();
    let mut edge_off = Vec::with_capacity(c.chunks.len());
    let mut o = 0usize;
    for &(_, _, l) in &c.chunks {
        let e = EDGE.min(l as usize);
        edge_off.push((o, e));
        o += 2 * e;
    }
    let k = needle.len().saturating_sub(1);
    let mut pair_hit: HashMap<(u32, u32), bool> = HashMap::new();
    let mut hits = 0;
    let mut win = Vec::with_capacity(2 * EDGE);
    for r in &c.refs {
        let mut hit = r.iter().any(|&id| chunk_hit[id as usize]);
        if !hit && k > 0 {
            for w in r.windows(2) {
                let e = pair_hit.entry((w[0], w[1])).or_insert_with(|| {
                    let (ao, ae) = edge_off[w[0] as usize];
                    let (bo, be) = edge_off[w[1] as usize];
                    let tail = &edges[ao + ae..ao + 2 * ae];
                    let head = &edges[bo..bo + be];
                    win.clear();
                    win.extend_from_slice(&tail[tail.len().saturating_sub(k)..]);
                    win.extend_from_slice(&head[..k.min(head.len())]);
                    finder.find(&win).is_some()
                });
                if *e {
                    hit = true;
                    break;
                }
            }
        }
        if hit {
            hits += 1;
        }
    }
    drop(dec_cache);
    let search_s = t.elapsed().as_secs_f64();
    Stats {
        raw,
        stored,
        encode_s,
        decode_s,
        lookup_bytes,
        lookup_s,
        lookups: picks.len() as u64,
        search_s,
        search_hits: hits,
        search_frames_read: frames_read,
        frames: c.frames.len() as u64,
    }
}

pub fn run(columns: &[Column], needle: &[u8]) {
    type Scheme<'a> = (&'a str, Box<dyn Fn(&Column, &[usize]) -> Stats + 'a>);
    let schemes: Vec<Scheme> = vec![
        ("page1m-zstd3", Box::new(|c, p| blocked_stats(c, 1 << 20, p, needle, false))),
        ("block64k-zstd3", Box::new(|c, p| blocked_stats(c, 64 << 10, p, needle, false))),
        ("block64k-zstd3-dict", Box::new(|c, p| blocked_stats(c, 64 << 10, p, needle, true))),
        ("cdc-f64k-zstd3", Box::new(|c, p| cdc_stats(c, p, needle, 3, false, 64 << 10, 1 << 16))),
        ("cdc-f256k-zstd3", Box::new(|c, p| cdc_stats(c, p, needle, 3, false, 256 << 10, 1 << 16))),
        ("cdc-f1m-zstd3", Box::new(|c, p| cdc_stats(c, p, needle, 3, false, 1 << 20, 1 << 16))),
        ("cdc-f256k-zstd3+tri", Box::new(|c, p| cdc_stats(c, p, needle, 3, true, 256 << 10, 1 << 16))),
        ("cdc-f64k-zstd3+tri2k", Box::new(|c, p| cdc_stats(c, p, needle, 3, true, 64 << 10, 1 << 14))),
        ("cdc-f256k-zstd9", Box::new(|c, p| cdc_stats(c, p, needle, 9, false, 256 << 10, 1 << 16))),
    ];
    println!("{:<22} {:>9} {:>9} {:>7} {:>9} {:>9} {:>11} {:>10} {:>9} {:>6} {:>12}", "scheme", "raw MiB", "stored", "ratio", "enc MB/s", "dec MB/s", "lookup KiB", "lookup us", "search ms", "hits", "frames read");
    for (name, f) in &schemes {
        let mut tot = Stats::default();
        for c in columns {
            let n = c.values.len();
            let picks: Vec<usize> = (0..200).map(|i| (i * 7919 + 13) % n.max(1)).filter(|&r| !c.values[r].is_empty()).collect();
            let s = f(c, &picks);
            tot.add(&s);
        }
        let mb = tot.raw as f64 / 1e6;
        println!(
            "{name:<22} {:>9.1} {:>9.1} {:>7.1} {:>9.0} {:>9.0} {:>11.1} {:>10.1} {:>9.1} {:>6} {:>5}/{:<6}",
            tot.raw as f64 / 1048576.0,
            tot.stored as f64 / 1048576.0,
            tot.raw as f64 / tot.stored as f64,
            mb / tot.encode_s,
            mb / tot.decode_s,
            tot.lookup_bytes as f64 / tot.lookups as f64 / 1024.0,
            tot.lookup_s / tot.lookups as f64 * 1e6,
            tot.search_s * 1000.0,
            tot.search_hits,
            tot.search_frames_read,
            tot.frames,
        );
    }
}
