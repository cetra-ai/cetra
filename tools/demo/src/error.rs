//! Errors of the generator. Messages name ids, paths and counts, never payloads or keys.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub(crate) enum DemoError {
    #[error("could not read {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("found {found} usable fixtures under the fixtures directory, need at least {needed}")]
    TooFewTemplates { found: usize, needed: usize },
    #[error("a timestamp is outside the range of nanoseconds since the Unix epoch")]
    Clock,
    #[error("HTTP request failed")]
    Http(#[from] reqwest::Error),
    #[error("could not encode a trace as JSON")]
    Encode(#[from] serde_json::Error),
    #[error("the server answered {status}")]
    Rejected { status: u16 },
    #[error("the judge endpoint could not bind or serve")]
    Judge(#[source] std::io::Error),
    #[error("--judge-addr is set, so --judge-key must be set too")]
    JudgeKeyMissing,
}
