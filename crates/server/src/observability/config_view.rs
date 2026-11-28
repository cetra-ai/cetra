//! The configuration as the debug bundle shows it (D-TL-6). The merged sources (the file, then `CETRA_*`) are walked as a key tree. A leaf shows its value only when its dotted path is in [`SHOWN_KEYS`]; every other leaf reads `"<redacted>"`, so a key added later is hidden until someone allow-lists it. Shown strings go through the scrubber and are cut at 4096 bytes. A load error becomes its key path and kind, never its text (figment and serde texts can quote values).

use figment::error::Kind;
use figment::value::{Dict, Num, Value as FigmentValue};
use serde::Serialize;
use serde_json::{Map, Value};

use super::scrub::{REDACTED, scrub_text};
use crate::config::ConfigError;

/// Keys whose value the bundle shows: every key of the configuration reference except those
/// that can hold a secret or an email address. Host names, bucket names, paths and domains are
/// shown (design answer 3: the user reviews the file).
pub(crate) const SHOWN_KEYS: &[&str] = &[
    "data_dir",
    "public_url",
    "http_addr",
    "otlp_http_addr",
    "grpc_addr",
    "metrics_addr",
    "trusted_proxies",
    "restore_mode",
    "storage.min_free_bytes",
    "storage.object_store",
    "storage.s3.region",
    "storage.s3.endpoint",
    "storage.s3.force_path_style",
    "storage.s3.allow_http",
    "storage.wal.sync",
    "storage.wal.segment_bytes",
    "storage.wal.group_max_bytes",
    "storage.wal.group_max_wait",
    "storage.wal.skip_corrupt",
    "storage.flush.max_bytes",
    "storage.flush.max_age",
    "storage.flush.upload_concurrency",
    "storage.memtable.hard_limit",
    "storage.query.target_partitions",
    "storage.query.metadata_cache_bytes",
    "storage.query.trace_lookback",
    "storage.query.max_memory_total",
    "storage.query.max_memory",
    "storage.query.initial_memory",
    "storage.query.admission_wait",
    "storage.query.read_cache_bytes",
    "storage.anchor.window",
    "storage.gc.grace",
    "storage.compaction.interval",
    "storage.compaction.threads",
    "storage.compaction.target_file_size",
    "storage.compaction.size_ratio",
    "storage.compaction.l0_files",
    "storage.compaction.l0_bytes",
    "storage.compaction.l0_age",
    "storage.compaction.close_delay",
    "storage.compaction.max_memory",
    "storage.deletion.max_delay",
    "ingest.max_request_bytes",
    "ingest.max_spans_per_request",
    "ingest.max_attribute_bytes",
    "ingest.max_inflight_bytes",
    "ingest.max_grpc_message_bytes",
    "media.min_bytes",
    "media.max_bytes",
    "eval.workers",
    "audit.retention_days",
    "issues.workers",
    "security.allow_private_network_egress",
    "entitlements.upgrade_url",
    "entitlements.control_plane_provider",
    "entitlements.control_plane",
    "entitlements.defaults.plan_name",
    "entitlements.defaults.features",
    "entitlements.defaults.units_included",
    "entitlements.defaults.units_cap",
    "entitlements.defaults.ingest_spans_per_sec",
    "entitlements.defaults.ingest_burst",
    "entitlements.defaults.data_access_days",
    "entitlements.defaults.max_seats",
    "entitlements.defaults.max_projects",
    "entitlements.defaults.max_alert_rules",
    "entitlements.defaults.max_active_online_rules",
    "entitlements.defaults.max_active_issue_sources",
    "entitlements.defaults.max_annotation_queues",
    "alerts.email_recipients",
    "orgs.user_create",
    "orgs.create_url",
    "demo.org",
    "demo.requests_per_minute",
    "demo.user_in_flight",
    "demo.org_in_flight",
    "demo.sql_timeout",
    "demo.sql_max_rows",
    "auth.signup",
    "auth.signup_domains",
    "auth.disable_password",
    "auth.require_email_verification",
    "auth.enforce_sso_domains",
    "auth.github.client_id",
    "auth.github.link_existing_by_email",
    "auth.google.client_id",
    "auth.google.allowed_domains",
    "auth.google.link_existing_by_email",
    "auth.oidc[].name",
    "auth.oidc[].display_name",
    "auth.oidc[].issuer",
    "auth.oidc[].client_id",
    "auth.oidc[].scopes",
    "auth.oidc[].allowed_domains",
    "auth.oidc[].auto_provision.org",
    "auth.oidc[].auto_provision.role",
    "auth.oidc[].link_existing_by_email",
    "init.org",
    "init.project",
    "log.format",
    "log.level",
    "self_monitoring.logs",
    "self_monitoring.level",
    "self_monitoring.resource",
    "telemetry.heartbeat",
    "telemetry.endpoint",
    "self_monitoring.traces",
    "self_monitoring.trace_sample_ratio",
    "telemetry.crash_reports",
    "analytics.posthog_host",
];

/// The longest shown string, in bytes; a longer one is cut and ends with `…<truncated>`.
const MAX_VALUE_BYTES: usize = 4096;

/// The `debug bundle` error kinds: what a failed load was, never its text (track TL § 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ConfigErrorKind {
    MissingFile,
    UnreadableFile,
    TomlSyntax,
    InvalidType,
    InvalidValue,
    InvalidLength,
    UnknownVariant,
    UnknownField,
    MissingField,
    DuplicateField,
    OutOfRange,
    Unsupported,
    Message,
    Invalid,
    Provider,
}

/// A failed load as the bundle shows it: a kind, the dotted key path (or the variable name for
/// `provider`) and the static rule of `invalid` and `provider`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ConfigErrorView {
    pub(crate) kind: ConfigErrorKind,
    /// The dotted key path, or the variable name for `provider`.
    pub(crate) key: Option<String>,
    /// The static rule of `invalid` and `provider` (never input).
    pub(crate) reason: Option<&'static str>,
}

/// The merged sources as redacted JSON.
pub(crate) fn redacted_values(tree: &Dict) -> Value {
    let json = Value::Object(
        tree.iter()
            .map(|(key, value)| (key.clone(), figment_to_json(value)))
            .collect(),
    );
    redact(&json)
}

/// A figment value as JSON. A number JSON cannot hold exactly becomes its decimal text.
fn figment_to_json(value: &FigmentValue) -> Value {
    match value {
        FigmentValue::String(_, text) => Value::String(text.clone()),
        FigmentValue::Char(_, c) => Value::String(c.to_string()),
        FigmentValue::Bool(_, b) => Value::Bool(*b),
        FigmentValue::Num(_, num) => num_to_json(*num),
        FigmentValue::Empty(_, _) => Value::Null,
        FigmentValue::Dict(_, dict) => Value::Object(
            dict.iter()
                .map(|(key, child)| (key.clone(), figment_to_json(child)))
                .collect(),
        ),
        FigmentValue::Array(_, items) => Value::Array(items.iter().map(figment_to_json).collect()),
    }
}

fn num_to_json(num: Num) -> Value {
    if let Some(float) = num.to_f64() {
        return serde_json::Number::from_f64(float).map_or(Value::Null, Value::Number);
    }
    if let Some(unsigned) = num.to_u128_lossy() {
        return u64::try_from(unsigned).map_or_else(
            |_| Value::String(unsigned.to_string()),
            |n| Value::Number(n.into()),
        );
    }
    if let Some(signed) = num.to_i128() {
        return i64::try_from(signed).map_or_else(
            |_| Value::String(signed.to_string()),
            |n| Value::Number(n.into()),
        );
    }
    Value::Null
}

/// `tree` with every leaf outside [`SHOWN_KEYS`] replaced by [`REDACTED`].
pub(crate) fn redact(tree: &Value) -> Value {
    redact_at("", tree)
}

/// Objects are walked key by key. An array that holds an object or an array is walked
/// element by element under `path[]`; an array of scalars is one leaf. Anything else is a
/// leaf: shown when `path` is allow-listed, else [`REDACTED`] (fail closed: a dict where a
/// scalar key is expected is walked, and its children are not on the list).
fn redact_at(path: &str, value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, child)| (key.clone(), redact_at(&join(path, key), child)))
                .collect::<Map<String, Value>>(),
        ),
        Value::Array(items) if items.iter().any(|item| item.is_object() || item.is_array()) => {
            let element = format!("{path}[]");
            Value::Array(items.iter().map(|item| redact_at(&element, item)).collect())
        }
        leaf => {
            if SHOWN_KEYS.contains(&path) {
                show(leaf)
            } else {
                Value::String(REDACTED.to_owned())
            }
        }
    }
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_owned()
    } else {
        format!("{path}.{key}")
    }
}

/// A shown leaf: strings scrubbed and capped. A string that parses as a URL loses its query
/// string and fragment after the scrubber: a token can sit in either (`?token=…`, `#…`).
fn show(value: &Value) -> Value {
    match value {
        Value::String(text) => {
            Value::String(cap(drop_query_and_fragment(&scrub_text(text)).to_owned()))
        }
        Value::Array(items) => Value::Array(items.iter().map(show).collect()),
        other => other.clone(),
    }
}

/// `text` without its query string and fragment when it parses as a URL, else `text` as is.
/// Runs after [`scrub_text`](super::scrub::scrub_text), so a `?` or `#` inside user info has
/// already been redacted; whatever is left is a real delimiter, and the text is otherwise
/// kept byte for byte (no re-serialization).
fn drop_query_and_fragment(text: &str) -> &str {
    if url::Url::parse(text).is_err() {
        return text;
    }
    text.find(['?', '#']).map_or(text, |at| &text[..at])
}

fn cap(text: String) -> String {
    if text.len() <= MAX_VALUE_BYTES {
        return text;
    }
    let mut cut = MAX_VALUE_BYTES;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…<truncated>", &text[..cut])
}

/// `error` as a kind and a key path. Figment and serde texts are never read, except the
/// prefix that marks a TOML syntax error, whose text `map_figment` already cut to a position.
pub(crate) fn error_view(error: &ConfigError) -> ConfigErrorView {
    match error {
        ConfigError::MissingFile(_) => view(ConfigErrorKind::MissingFile, None, None),
        ConfigError::Invalid { key, reason } => view(
            ConfigErrorKind::Invalid,
            Some((*key).to_owned()),
            Some(*reason),
        ),
        ConfigError::Provider { key, reason } => {
            view(ConfigErrorKind::Provider, Some(key.clone()), Some(*reason))
        }
        ConfigError::Figment(error) => figment_view(error),
    }
}

/// A failure of [`crate::config::read_toml`]: the file is missing, or could not be read
/// (whose figment text is an I/O message, never shown).
pub(crate) fn read_error_view(error: &ConfigError) -> ConfigErrorView {
    match error {
        ConfigError::MissingFile(_) => view(ConfigErrorKind::MissingFile, None, None),
        _ => view(ConfigErrorKind::UnreadableFile, None, None),
    }
}

fn figment_view(error: &figment::Error) -> ConfigErrorView {
    let Some(part) = error.clone().into_iter().next() else {
        return view(ConfigErrorKind::Message, None, None);
    };
    let key = (!part.path.is_empty()).then(|| part.path.join("."));
    let kind = match &part.kind {
        Kind::Message(text) if part.path.is_empty() && text.starts_with("TOML parse error") => {
            ConfigErrorKind::TomlSyntax
        }
        Kind::Message(_) => ConfigErrorKind::Message,
        Kind::InvalidType(..) => ConfigErrorKind::InvalidType,
        Kind::InvalidValue(..) => ConfigErrorKind::InvalidValue,
        Kind::InvalidLength(..) => ConfigErrorKind::InvalidLength,
        Kind::UnknownVariant(..) => ConfigErrorKind::UnknownVariant,
        Kind::UnknownField(..) => ConfigErrorKind::UnknownField,
        Kind::MissingField(_) => ConfigErrorKind::MissingField,
        Kind::DuplicateField(_) => ConfigErrorKind::DuplicateField,
        Kind::ISizeOutOfRange(_) | Kind::USizeOutOfRange(_) => ConfigErrorKind::OutOfRange,
        Kind::Unsupported(_) | Kind::UnsupportedKey(..) => ConfigErrorKind::Unsupported,
    };
    view(kind, key, None)
}

fn view(
    kind: ConfigErrorKind,
    key: Option<String>,
    reason: Option<&'static str>,
) -> ConfigErrorView {
    ConfigErrorView { kind, key, reason }
}

#[cfg(test)]
mod tests {
    use figment::value::{Empty, Tag};
    use serde_json::json;
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    use super::*;
    use crate::config::{load_from, source_tree};

    /// Keys whose value the bundle never shows: secrets and email addresses.
    const REDACTED_KEYS: &[&str] = &[
        "encryption_key",
        "license_key",
        "smtp_url",
        "email_from",
        "auth.github.client_secret",
        "auth.google.client_secret",
        "auth.oidc[].client_secret",
        "init.admin_email",
        "init.admin_password",
        "init.secret_key",
        "init.read_key",
        "self_monitoring.otlp_headers",
        // Some vendors put a token in the endpoint's path (M1's `OtlpEndpoint` logs its host
        // only); a running server's `debug_info.self_monitoring.endpoint_host` gives the host.
        "self_monitoring.otlp_endpoint",
        "analytics.posthog_key",
    ];
    const REFERENCE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../website/src/content/docs/docs/self-hosting/configuration-reference.md"
    ));

    /// The keys of the configuration reference: the text between the first two backticks of
    /// each line that starts with ``| ` ``.
    fn reference_keys() -> BTreeSet<String> {
        REFERENCE
            .lines()
            .filter_map(|line| {
                let rest = line.strip_prefix("| `")?;
                let (key, _) = rest.split_once('`')?;
                Some(key.to_owned())
            })
            .collect()
    }

    /// A JSON object built from dotted paths. A segment ending in `[]` becomes an array holding
    /// one object; later paths under the same `x[]` go into that same object.
    fn tree_with(paths: &[(&str, Value)]) -> Value {
        let mut root = Map::new();
        for (path, value) in paths {
            insert_path(&mut root, path, value.clone());
        }
        Value::Object(root)
    }

    fn insert_path(map: &mut Map<String, Value>, path: &str, value: Value) {
        let Some((head, rest)) = path.split_once('.') else {
            map.insert(path.to_owned(), value);
            return;
        };
        if let Some(name) = head.strip_suffix("[]") {
            let slot = map
                .entry(name.to_owned())
                .or_insert_with(|| Value::Array(vec![Value::Object(Map::new())]));
            if let Value::Array(items) = slot {
                if items.is_empty() {
                    items.push(Value::Object(Map::new()));
                }
                if let Some(Value::Object(inner)) = items.first_mut() {
                    insert_path(inner, rest, value);
                }
            }
        } else {
            let slot = map
                .entry(head.to_owned())
                .or_insert_with(|| Value::Object(Map::new()));
            if let Value::Object(inner) = slot {
                insert_path(inner, rest, value);
            }
        }
    }

    /// The value at a dotted path; `x[]` reads the first element of `x`.
    fn read<'a>(value: &'a Value, path: &str) -> &'a Value {
        path.split('.')
            .fold(value, |current, segment| match segment.strip_suffix("[]") {
                Some(name) => &current[name][0],
                None => &current[segment],
            })
    }

    fn redacted() -> Value {
        Value::String(REDACTED.to_owned())
    }

    #[test]
    fn every_reference_key_is_classified() {
        let reference = reference_keys();
        let shown: BTreeSet<&str> = SHOWN_KEYS.iter().copied().collect();
        let hidden: BTreeSet<&str> = REDACTED_KEYS.iter().copied().collect();
        assert_eq!(
            shown.len(),
            SHOWN_KEYS.len(),
            "SHOWN_KEYS has a duplicate, in observability/config_view.rs"
        );
        assert_eq!(
            hidden.len(),
            REDACTED_KEYS.len(),
            "REDACTED_KEYS has a duplicate, in observability/config_view.rs"
        );
        for key in &reference {
            let in_shown = shown.contains(key.as_str());
            let in_hidden = hidden.contains(key.as_str());
            assert!(
                !(in_shown && in_hidden),
                "{key} is in both SHOWN_KEYS and REDACTED_KEYS: keep it in one, in observability/config_view.rs"
            );
            assert!(
                in_shown || in_hidden,
                "{key} is not classified: classify it in observability/config_view.rs (SHOWN_KEYS or the test's REDACTED_KEYS)"
            );
        }
        for key in shown.iter().chain(hidden.iter()) {
            assert!(
                reference.contains(*key),
                "{key} is in observability/config_view.rs but not on the configuration reference page: remove it from the list"
            );
        }
    }

    #[test]
    fn a_planted_value_in_every_secret_key_never_appears() {
        let paths: Vec<(&str, Value)> = REDACTED_KEYS
            .iter()
            .enumerate()
            .map(|(i, key)| {
                let value = if matches!(*key, "init.admin_email" | "email_from") {
                    Value::String(format!("planted{i}@leak.example"))
                } else {
                    Value::String(format!("PLANTED-{i}-sekret"))
                };
                (*key, value)
            })
            .collect();
        let out = redact(&tree_with(&paths));
        let text = out.to_string();
        assert!(!text.contains("PLANTED"), "a secret value leaked: {text}");
        assert!(!text.contains("leak.example"), "an email leaked: {text}");
        for key in REDACTED_KEYS {
            assert_eq!(read(&out, key), &redacted(), "{key}");
        }
    }

    #[test]
    fn every_shown_key_shows_its_value() {
        let paths: Vec<(&str, Value)> = SHOWN_KEYS
            .iter()
            .enumerate()
            .map(|(i, key)| (*key, Value::String(format!("v-{i}"))))
            .collect();
        let out = redact(&tree_with(&paths));
        for (i, key) in SHOWN_KEYS.iter().enumerate() {
            assert_eq!(
                read(&out, key),
                &Value::String(format!("v-{i}")),
                "{key} should show its value"
            );
        }
    }

    #[test]
    fn an_unknown_key_is_redacted_and_named() {
        let tree = json!({"zz_unknown": "x", "log": {"level": "info", "extra": 1}});
        let out = redact(&tree);
        assert_eq!(read(&out, "zz_unknown"), &redacted());
        assert_eq!(read(&out, "log.extra"), &redacted());
        assert_eq!(read(&out, "log.level"), &json!("info"));
    }

    #[test]
    fn a_table_where_a_shown_scalar_belongs_is_redacted() {
        let out = redact(&json!({"log": {"level": {"a": "PLANTED"}}}));
        assert_eq!(read(&out, "log.level.a"), &redacted());
        assert!(!out.to_string().contains("PLANTED"));
    }

    #[test]
    fn arrays_of_scalars_are_one_leaf() {
        let out = redact(&json!({
            "trusted_proxies": ["10.0.0.0/8"],
            "init": {"secret_key": ["PLANTED"]},
        }));
        assert_eq!(read(&out, "trusted_proxies"), &json!(["10.0.0.0/8"]));
        assert_eq!(read(&out, "init.secret_key"), &redacted());
    }

    #[test]
    fn shown_strings_are_scrubbed_and_capped() {
        let long = "a".repeat(5000);
        let out = redact(&tree_with(&[
            ("init.org", Value::String("ada@leak.example".to_owned())),
            (
                "storage.s3.endpoint",
                Value::String("https://u:PLANTED@minio.example".to_owned()),
            ),
            ("log.level", Value::String(long)),
        ]));
        assert_eq!(read(&out, "init.org"), &redacted());
        assert_eq!(
            read(&out, "storage.s3.endpoint"),
            &json!("https://<redacted>@minio.example")
        );
        let capped = format!("{}…<truncated>", "a".repeat(MAX_VALUE_BYTES));
        assert_eq!(read(&out, "log.level"), &json!(capped));
    }

    #[test]
    fn url_shown_values_lose_their_query_and_fragment() {
        // A token can sit after `?` or `#` in any shown URL; the scrubber alone leaves it.
        let out = redact(&tree_with(&[
            (
                "orgs.create_url",
                Value::String("https://billing.example/new?token=PLANTED".to_owned()),
            ),
            (
                "public_url",
                Value::String("https://app.example/#PLANTED".to_owned()),
            ),
            (
                "entitlements.upgrade_url",
                Value::String("https://shop.example/up?team=1#PLANTED".to_owned()),
            ),
            (
                "storage.s3.endpoint",
                Value::String("https://minio.example:9000?accessKey=PLANTED#frag".to_owned()),
            ),
            (
                "auth.oidc[].issuer",
                Value::String("https://id.example.com/realms/x?hint=PLANTED".to_owned()),
            ),
            // Not a URL: a `?` in plain text is kept.
            ("init.org", Value::String("What? yes".to_owned())),
        ]));
        let text = out.to_string();
        assert!(
            !text.contains("PLANTED"),
            "a query or fragment leaked: {text}"
        );
        assert_eq!(
            read(&out, "orgs.create_url"),
            &json!("https://billing.example/new")
        );
        assert_eq!(read(&out, "public_url"), &json!("https://app.example/"));
        assert_eq!(
            read(&out, "entitlements.upgrade_url"),
            &json!("https://shop.example/up")
        );
        assert_eq!(
            read(&out, "storage.s3.endpoint"),
            &json!("https://minio.example:9000")
        );
        assert_eq!(
            read(&out, "auth.oidc[].issuer"),
            &json!("https://id.example.com/realms/x")
        );
        assert_eq!(read(&out, "init.org"), &json!("What? yes"));
    }

    #[test]
    fn a_delimiter_inside_user_info_still_scrubs_whole() {
        // The query and fragment are dropped after the scrubber, so a `#` inside user info
        // does not cut the password short of its `@`.
        let out = redact(&tree_with(&[(
            "storage.s3.endpoint",
            Value::String("https://u:p#PLANTED@minio.example?x=1".to_owned()),
        )]));
        assert_eq!(
            read(&out, "storage.s3.endpoint"),
            &json!("https://<redacted>@minio.example")
        );
    }

    #[test]
    fn numbers_and_booleans_convert() {
        assert_eq!(
            figment_to_json(&FigmentValue::Num(Tag::Default, Num::U8(3))),
            json!(3)
        );
        assert_eq!(
            figment_to_json(&FigmentValue::Num(Tag::Default, Num::I64(-4))),
            json!(-4)
        );
        assert_eq!(
            figment_to_json(&FigmentValue::Num(Tag::Default, Num::U128(u128::MAX))),
            json!("340282366920938463463374607431768211455")
        );
        assert_eq!(
            figment_to_json(&FigmentValue::Num(Tag::Default, Num::F64(0.5))),
            json!(0.5)
        );
        assert_eq!(
            figment_to_json(&FigmentValue::Bool(Tag::Default, true)),
            json!(true)
        );
        assert_eq!(
            figment_to_json(&FigmentValue::Empty(Tag::Default, Empty::None)),
            json!(null)
        );
    }

    #[test]
    fn source_tree_merges_the_file_and_the_environment() {
        let toml = "encryption_key = \"PLANTED-file\"\n[log]\nlevel = \"info\"\n";
        let env = [
            ("CETRA_LOG", "debug"),
            ("CETRA_SMTP_URL", "smtp://u:PLANTED@h"),
        ];
        let tree = source_tree(Some(toml), &env).expect("the sources merge");
        let values = redacted_values(&tree);
        assert_eq!(read(&values, "log.level"), &json!("debug"));
        assert_eq!(read(&values, "encryption_key"), &redacted());
        assert_eq!(read(&values, "smtp_url"), &redacted());
        assert!(!values.to_string().contains("PLANTED"));
    }

    #[test]
    fn oidc_client_secrets_from_the_environment_are_not_in_the_tree() {
        let env = [("CETRA_AUTH__OIDC__OKTA__CLIENT_SECRET", "PLANTED")];
        let tree = source_tree(None, &env).expect("the sources merge");
        assert!(!redacted_values(&tree).to_string().contains("PLANTED"));
    }

    #[test]
    fn cetra_telemetry_is_not_in_the_tree() {
        let env = [("CETRA_TELEMETRY", "off")];
        let tree = source_tree(None, &env).expect("the sources merge");
        assert!(!tree.contains_key("telemetry"));
    }

    #[test]
    fn error_view_never_quotes_a_value() {
        let cases: [(&str, &[(&str, &str)]); 3] = [
            ("http_addr = \"PLANTED-nope\"\n", &[]),
            ("[storage.wal]\nsync = \"PLANTED\"\n", &[]),
            ("smtp_url = \"PLANTED://x\"\n", &[]),
        ];
        for (toml, env) in cases {
            let error = load_from(Some(toml), env).expect_err("the configuration is rejected");
            let text = serde_json::to_string(&error_view(&error)).expect("the view serializes");
            assert!(!text.contains("PLANTED"), "{text}");
        }

        let error = load_from(Some("http_addr = \"PLANTED-nope\"\n"), &[])
            .expect_err("the configuration is rejected");
        assert_eq!(error_view(&error).key.as_deref(), Some("http_addr"));

        let error = load_from(Some("[storage.wal]\nsync = \"PLANTED\"\n"), &[])
            .expect_err("the configuration is rejected");
        let view = error_view(&error);
        assert_eq!(view.key.as_deref(), Some("storage.wal.sync"));
        assert!(matches!(
            view.kind,
            ConfigErrorKind::UnknownVariant | ConfigErrorKind::Message
        ));

        let error = load_from(Some("smtp_url = \"PLANTED://x\"\n"), &[])
            .expect_err("the configuration is rejected");
        let view = error_view(&error);
        assert_eq!(view.kind, ConfigErrorKind::Invalid);
        assert_eq!(view.key.as_deref(), Some("smtp_url"));
        assert!(matches!(
            view.reason,
            Some("invalid URL") | Some("expected a string")
        ));
    }

    #[test]
    fn a_toml_syntax_error_is_toml_syntax() {
        let error =
            source_tree(Some("encryption_key = \"PLANTED"), &[]).expect_err("the TOML is rejected");
        let view = error_view(&error);
        assert_eq!(view.kind, ConfigErrorKind::TomlSyntax);
        assert_eq!(view.key, None);
        assert!(
            !serde_json::to_string(&view)
                .expect("the view serializes")
                .contains("PLANTED")
        );
    }

    #[test]
    fn an_unknown_key_is_unknown_field() {
        let error =
            load_from(Some("zz_probe = 1\n"), &[]).expect_err("the configuration is rejected");
        let view = error_view(&error);
        assert_eq!(view.kind, ConfigErrorKind::UnknownField);
        assert!(view.key.is_some_and(|key| key.contains("zz_probe")));
    }

    #[test]
    fn a_bad_cetra_telemetry_is_a_provider_error() {
        let error = load_from(None, &[("CETRA_TELEMETRY", "PLANTED")])
            .expect_err("the configuration is rejected");
        let view = error_view(&error);
        assert_eq!(view.kind, ConfigErrorKind::Provider);
        assert_eq!(view.key.as_deref(), Some("CETRA_TELEMETRY"));
        assert!(
            !serde_json::to_string(&view)
                .expect("the view serializes")
                .contains("PLANTED")
        );
    }

    #[test]
    fn read_errors_have_their_own_kinds() {
        assert_eq!(
            read_error_view(&ConfigError::MissingFile(PathBuf::from("/x"))).kind,
            ConfigErrorKind::MissingFile
        );
        assert_eq!(
            read_error_view(&ConfigError::Figment(Box::new(figment::Error::from(
                "x".to_owned()
            ))))
            .kind,
            ConfigErrorKind::UnreadableFile
        );
    }

    #[test]
    fn error_kinds_serialize_snake_case() {
        assert_eq!(
            serde_json::to_string(&ConfigErrorKind::TomlSyntax).expect("serializes"),
            "\"toml_syntax\""
        );
        assert_eq!(
            serde_json::to_string(&ConfigErrorKind::UnknownField).expect("serializes"),
            "\"unknown_field\""
        );
    }
}
