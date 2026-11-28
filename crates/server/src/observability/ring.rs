//! The log ring buffer of `cetra serve` (track TL § 4): the last log lines at the stdout filter,
//! as JSON, kept in memory for the admin socket's `debug_info`. Bounded by line count, total
//! bytes and line length, so a burst or a huge field cannot grow it.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::Filter;
use tracing_subscriber::{Layer, Registry};

use super::subscriber::BoxedLayer;

/// Lines kept.
pub(crate) const MAX_LINES: usize = 2000;
/// Bytes of all kept lines together.
pub(crate) const MAX_BYTES: usize = 2 * 1024 * 1024;
/// Bytes of one line; a longer line is cut and ends with [`TRUNCATED`].
pub(crate) const MAX_LINE_BYTES: usize = 16 * 1024;
/// The end of a cut line.
pub(crate) const TRUNCATED: &str = "…<truncated>";
/// What one writer buffers for one event before the cut (the rest is discarded).
const WRITE_CAP: usize = 2 * MAX_LINE_BYTES;

/// The ring. Cloning shares it.
#[derive(Debug, Clone, Default)]
pub(crate) struct LogRing(Arc<Mutex<RingState>>);

#[derive(Debug, Default)]
struct RingState {
    lines: VecDeque<String>,
    /// Sum of `lines`' lengths; never above [`MAX_BYTES`].
    bytes: usize,
    /// Lines removed to make room since the ring was made.
    dropped: u64,
}

/// A copy of the ring's content, oldest line first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RingSnapshot {
    pub(crate) lines: Vec<String>,
    pub(crate) dropped: u64,
}

/// The ring of this process, set once by `subscriber::init`.
static INSTALLED: OnceLock<LogRing> = OnceLock::new();

/// Makes `ring` the process's ring. The first call wins; later calls change nothing.
pub(crate) fn install(ring: LogRing) {
    let _ = INSTALLED.set(ring);
}

/// The process's ring, if `subscriber::init` installed one.
pub(crate) fn installed() -> Option<&'static LogRing> {
    INSTALLED.get()
}

impl LogRing {
    /// An empty ring.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Keeps `line` (without its line ending), cut to [`MAX_LINE_BYTES`], and drops the oldest
    /// lines until the bounds hold. An empty line is not kept.
    pub(crate) fn push(&self, line: &str) {
        let line = line.trim_end_matches(['\n', '\r']);
        if line.is_empty() {
            return;
        }
        let line = truncate_line(line);
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        // Invariant: `MAX_LINE_BYTES < MAX_BYTES`, so once the loop ends
        // `bytes + line.len() <= MAX_BYTES`.
        while !state.lines.is_empty()
            && (state.lines.len() >= MAX_LINES || state.bytes + line.len() > MAX_BYTES)
        {
            if let Some(old) = state.lines.pop_front() {
                state.bytes -= old.len();
                state.dropped += 1;
            }
        }
        state.bytes += line.len();
        state.lines.push_back(line);
    }

    /// A copy of the content, oldest line first.
    pub(crate) fn snapshot(&self) -> RingSnapshot {
        let state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        RingSnapshot {
            lines: state.lines.iter().cloned().collect(),
            dropped: state.dropped,
        }
    }

    /// A `tracing` layer that writes each event as one JSON line into this ring, through
    /// `filter` (TL3.2 passes the stdout filter combined with a target filter).
    pub(crate) fn layer<F>(&self, filter: F) -> BoxedLayer
    where
        F: Filter<Registry> + Send + Sync + 'static,
    {
        tracing_subscriber::fmt::layer()
            .json()
            .with_current_span(false)
            .with_span_list(false)
            .with_ansi(false)
            .with_writer(self.clone())
            .with_filter(filter)
            .boxed()
    }
}

/// `line` unchanged when it fits [`MAX_LINE_BYTES`]; otherwise cut on a character boundary and
/// ending with [`TRUNCATED`].
fn truncate_line(line: &str) -> String {
    if line.len() <= MAX_LINE_BYTES {
        return line.to_owned();
    }
    let mut cut = MAX_LINE_BYTES - TRUNCATED.len();
    while !line.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{TRUNCATED}", &line[..cut])
}

/// Buffers one event's formatted text and pushes its lines into the ring when dropped
/// (the fmt layer writes one event per writer).
pub(crate) struct RingWriter {
    ring: LogRing,
    buf: Vec<u8>,
}

impl Write for RingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let room = WRITE_CAP.saturating_sub(self.buf.len());
        self.buf.extend_from_slice(&bytes[..bytes.len().min(room)]);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for RingWriter {
    fn drop(&mut self) {
        let text = String::from_utf8_lossy(&self.buf);
        for line in text.split('\n') {
            self.ring.push(line);
        }
    }
}

impl<'a> MakeWriter<'a> for LogRing {
    type Writer = RingWriter;

    fn make_writer(&'a self) -> Self::Writer {
        RingWriter {
            ring: self.clone(),
            buf: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::filter::EnvFilter;
    use tracing_subscriber::layer::SubscriberExt;

    #[test]
    fn push_keeps_lines_in_order() {
        let ring = LogRing::new();
        ring.push("a");
        ring.push("b");
        ring.push("c");
        let snapshot = ring.snapshot();
        assert_eq!(snapshot.lines, ["a", "b", "c"]);
        assert_eq!(snapshot.dropped, 0);
    }

    #[test]
    fn empty_lines_and_newlines_are_not_kept() {
        let ring = LogRing::new();
        ring.push("");
        ring.push("\n");
        ring.push("x\r\n");
        assert_eq!(ring.snapshot().lines, ["x"]);
    }

    #[test]
    fn the_line_count_is_bounded() {
        let ring = LogRing::new();
        for i in 0..2500 {
            ring.push(&format!("line-{i}"));
        }
        let snapshot = ring.snapshot();
        assert_eq!(snapshot.lines.len(), 2000);
        assert_eq!(snapshot.lines[0], "line-500");
        assert_eq!(snapshot.dropped, 500);
    }

    #[test]
    fn the_byte_total_is_bounded() {
        let ring = LogRing::new();
        let long = "x".repeat(10_000);
        for i in 0..300 {
            ring.push(&format!("{i:05}{long}"));
        }
        let snapshot = ring.snapshot();
        let total: usize = snapshot.lines.iter().map(String::len).sum();
        assert!(total <= MAX_BYTES);
        let last = snapshot.lines.last().map(String::as_str).unwrap_or("");
        assert!(last.starts_with("00299"));
        assert_eq!(snapshot.dropped, 300 - snapshot.lines.len() as u64);
    }

    #[test]
    fn a_long_line_is_cut_on_a_char_boundary() {
        let ring = LogRing::new();
        ring.push(&"é".repeat(20_000));
        let snapshot = ring.snapshot();
        let line = &snapshot.lines[0];
        assert!(line.len() <= MAX_LINE_BYTES);
        assert!(line.ends_with(TRUNCATED));

        let exact = "y".repeat(MAX_LINE_BYTES);
        let ring = LogRing::new();
        ring.push(&exact);
        assert_eq!(ring.snapshot().lines, [exact]);
    }

    #[test]
    fn concurrent_pushes_keep_the_bounds() {
        let ring = LogRing::new();
        let line = "z".repeat(600);
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let ring = ring.clone();
                let line = line.clone();
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        ring.push(&line);
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        let snapshot = ring.snapshot();
        assert!(snapshot.lines.len() <= MAX_LINES);
        let total: usize = snapshot.lines.iter().map(String::len).sum();
        assert!(total <= MAX_BYTES);
        assert_eq!(snapshot.lines.len() as u64 + snapshot.dropped, 8000);
    }

    #[test]
    fn the_layer_writes_json_lines_without_span_fields() {
        let ring = LogRing::new();
        let subscriber = tracing_subscriber::registry().with(ring.layer(EnvFilter::new("info")));
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("outer", id = 7);
            let _entered = span.enter();
            tracing::info!(project_id = 7, "hello");
            tracing::debug!("hidden");
        });
        let lines = ring.snapshot().lines;
        assert_eq!(lines.len(), 1);
        let value: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(value["fields"]["message"], "hello");
        assert_eq!(value["fields"]["project_id"], 7);
        assert!(value.get("span").is_none());
        assert!(value.get("spans").is_none());
    }

    #[test]
    fn the_writer_caps_one_event() {
        let ring = LogRing::new();
        let mut writer = ring.make_writer();
        writer.write_all(&vec![b'a'; 100_000]).unwrap();
        drop(writer);
        let lines = ring.snapshot().lines;
        assert_eq!(lines.len(), 1);
        assert!(lines[0].len() <= MAX_LINE_BYTES);
        assert!(lines[0].ends_with(TRUNCATED));
    }

    /// The only test that installs a ring: other tests never assume `installed()` is `None`,
    /// because one process runs every server test.
    #[test]
    fn install_keeps_the_first_ring() {
        let a = LogRing::new();
        install(a.clone());
        install(LogRing::new());
        a.push("kept");
        let installed = installed().unwrap();
        assert!(installed.snapshot().lines.iter().any(|l| l == "kept"));
    }
}
