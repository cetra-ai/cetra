//! The storage engine's spans (track TL M2, D-TL-4): the names and attribute keys are
//! constants, built here by one constructor each. Values are table names, ids, counts and
//! fixed words: never a path, an object key or an error's text. `tracing` is a facade: these
//! spans cost little when no layer records them, and the server decides whether they are
//! exported.

use tracing::Span;
use tracing::field::Empty;

pub(crate) const TABLE: &str = "table";
pub(crate) const PROJECT_ID: &str = "project_id";
pub(crate) const KIND: &str = "kind";
pub(crate) const PARTITIONS: &str = "partitions";
pub(crate) const FILES: &str = "files";
pub(crate) const BYTES: &str = "bytes";
/// `ok` or why not.
pub(crate) const OUTCOME: &str = "outcome";

/// `value` as an `i64` attribute, `i64::MAX` when it does not fit (OTel layers record a `u64`
/// as a string).
pub(crate) fn count<T: TryInto<i64>>(value: T) -> i64 {
    value.try_into().unwrap_or(i64::MAX)
}

/// One flush of `table` that froze rows (`flush::flush_once`).
pub(crate) fn storage_flush(table: &'static str) -> Span {
    tracing::info_span!(
        "storage.flush",
        { TABLE } = table,
        { PARTITIONS } = Empty,
        { FILES } = Empty,
        { BYTES } = Empty,
        { OUTCOME } = Empty
    )
}

/// One compaction job (`compaction::job::Compactor::pass`). `kind` is the job kind label.
pub(crate) fn storage_compaction(table: &'static str, project: &str, kind: &'static str) -> Span {
    tracing::info_span!(
        "storage.compaction",
        { TABLE } = table,
        { PROJECT_ID } = project,
        { KIND } = kind,
        { FILES } = Empty,
        { BYTES } = Empty,
        { OUTCOME } = Empty
    )
}

/// One periodic garbage collection tick (`gc::spawn`).
pub(crate) fn storage_gc() -> Span {
    tracing::info_span!(
        "storage.gc",
        { FILES } = Empty,
        { BYTES } = Empty,
        { OUTCOME } = Empty
    )
}

/// A layer that records each span's name and fields when it closes (tests only).
#[cfg(test)]
pub(crate) mod capture {
    use std::collections::BTreeMap;
    use std::fmt;
    use std::sync::{Arc, Mutex};

    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing_subscriber::layer::{Context, Layer};
    use tracing_subscriber::registry::LookupSpan;

    /// A closed span: its name and its recorded fields, as text.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct Closed {
        pub(crate) name: &'static str,
        pub(crate) fields: BTreeMap<&'static str, String>,
    }

    /// Records every span it sees, with its fields, when the span closes.
    #[derive(Debug, Clone, Default)]
    pub(crate) struct Capture {
        closed: Arc<Mutex<Vec<Closed>>>,
    }

    impl Capture {
        /// The spans closed so far, in order.
        pub(crate) fn closed(&self) -> Vec<Closed> {
            self.closed.lock().expect("capture lock").clone()
        }
    }

    struct Fields(BTreeMap<&'static str, String>);

    impl Visit for Fields {
        fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
            self.0.insert(field.name(), format!("{value:?}"));
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.insert(field.name(), value.to_owned());
        }
    }

    impl<S> Layer<S> for Capture
    where
        S: tracing::Subscriber + for<'lookup> LookupSpan<'lookup>,
    {
        fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
            let mut fields = Fields(BTreeMap::new());
            attrs.record(&mut fields);
            if let Some(span) = ctx.span(id) {
                span.extensions_mut().insert(fields);
            }
        }

        fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
            if let Some(span) = ctx.span(id) {
                let mut extensions = span.extensions_mut();
                if let Some(fields) = extensions.get_mut::<Fields>() {
                    values.record(fields);
                }
            }
        }

        fn on_close(&self, id: Id, ctx: Context<'_, S>) {
            if let Some(span) = ctx.span(&id) {
                let fields = span
                    .extensions_mut()
                    .remove::<Fields>()
                    .map(|fields| fields.0)
                    .unwrap_or_default();
                self.closed.lock().expect("capture lock").push(Closed {
                    name: span.name(),
                    fields,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{count, storage_compaction, storage_flush, storage_gc};
    use tracing::Span;

    fn shape(span: &Span) -> (&'static str, Vec<&'static str>) {
        let meta = span.metadata().expect("a span keeps its metadata");
        (
            meta.name(),
            meta.fields().iter().map(|field| field.name()).collect(),
        )
    }

    #[test]
    fn the_storage_span_catalog_is_fixed() {
        let flush = storage_flush("spans");
        let compaction = storage_compaction("spans", "p", "l0-merge");
        let gc = storage_gc();

        assert_eq!(
            shape(&flush),
            (
                "storage.flush",
                vec!["table", "partitions", "files", "bytes", "outcome"]
            )
        );
        assert_eq!(
            shape(&compaction),
            (
                "storage.compaction",
                vec!["table", "project_id", "kind", "files", "bytes", "outcome"]
            )
        );
        assert_eq!(
            shape(&gc),
            ("storage.gc", vec!["files", "bytes", "outcome"])
        );
        for span in [&flush, &compaction, &gc] {
            assert_eq!(span.metadata().unwrap().target(), "cetra_storage::spans");
        }
    }

    #[test]
    fn count_saturates() {
        assert_eq!(count(u64::MAX), i64::MAX);
        assert_eq!(count(5_usize), 5);
    }
}
