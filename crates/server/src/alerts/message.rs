//! Notification bodies and links (spec 07 § Channels, D-K-8).

use std::fmt;

use serde::Serialize;
use url::form_urlencoded::Serializer;
use uuid::Uuid;

use cetra_core::ids::ProjectId;
use cetra_core::names::PRODUCT;
use cetra_core::time_serde;

use crate::api::alerts::{AlertMetric, AlertWindow, Comparator};
use crate::config::PublicUrl;
use crate::email::{Email, IssueLine, issues_noun};
use crate::meta::issue_links::{DIGEST_ITEMS, DigestItem, DigestKind};

/// Why a message is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MessageKind {
    Fired,
    Resolved,
    NoData,
    Reminder,
    Test,
}

/// Everything a message shows. Built from an event (`deliver_once`) or a sample (test send).
/// Never log a `Notice`: it holds rule and project names and URLs.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Notice {
    pub(crate) kind: MessageKind,
    /// The event id, or a fresh UUIDv7 for a test.
    pub(crate) event_id: Uuid,
    pub(crate) rule_id: Uuid,
    pub(crate) rule_name: String,
    pub(crate) metric: AlertMetric,
    pub(crate) comparator: Comparator,
    pub(crate) threshold: f64,
    pub(crate) window: AlertWindow,
    pub(crate) value: Option<f64>,
    pub(crate) count: Option<i64>,
    pub(crate) window_from: i64,
    pub(crate) window_to: i64,
    /// When the event was recorded.
    pub(crate) occurred_at: i64,
    pub(crate) project_id: ProjectId,
    pub(crate) project_name: String,
    pub(crate) rule_url: String,
    pub(crate) traces_url: String,
}

/// `<public_url>/<org_slug>/<project_slug>/alerts/<rule_id>`.
pub(crate) fn rule_url(
    public_url: &PublicUrl,
    org_slug: &str,
    project_slug: &str,
    rule_id: Uuid,
) -> String {
    public_url.join(&format!("/{org_slug}/{project_slug}/alerts/{rule_id}"))
}

/// `<public_url>/<org_slug>/<project_slug>/traces?from=<rfc3339>&to=<rfc3339>[&filter=<text>]`,
/// the query built with `url::form_urlencoded::Serializer` (so the filter is percent-encoded).
pub(crate) fn traces_url(
    public_url: &PublicUrl,
    org_slug: &str,
    project_slug: &str,
    from: i64,
    to: i64,
    filter: Option<&str>,
) -> String {
    let mut query = Serializer::new(String::new());
    query.append_pair("from", &rfc3339(from));
    query.append_pair("to", &rfc3339(to));
    if let Some(filter) = filter {
        query.append_pair("filter", filter);
    }
    public_url.join(&format!(
        "/{org_slug}/{project_slug}/traces?{}",
        query.finish()
    ))
}

fn rfc3339(ns: i64) -> String {
    let nanos = u32::try_from(ns.rem_euclid(1_000_000_000)).unwrap_or(0);
    chrono::DateTime::<chrono::Utc>::from_timestamp(ns.div_euclid(1_000_000_000), nanos)
        .map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true))
        .unwrap_or_default()
}

/// How a value reads in a message: `12.5%` for `error_rate`, `$3.75` (2 decimals) for `cost`,
/// `820 ms` (0 decimals) for `latency_p95`, 3 decimals for `score` (`0.700`), a whole count for
/// `issue_occurrences` (`12`). `None` → `"no data"`.
pub(crate) fn value_text(metric: AlertMetric, value: Option<f64>) -> String {
    let Some(value) = value else {
        return "no data".to_owned();
    };
    match metric {
        AlertMetric::ErrorRate => format!("{:.1}%", value * 100.0),
        AlertMetric::Cost => format!("${value:.2}"),
        AlertMetric::LatencyP95 => format!("{value:.0} ms"),
        AlertMetric::Score => format!("{value:.3}"),
        AlertMetric::IssueOccurrences => format!("{value:.0}"),
    }
}

/// The title of a message. The rule name is cut to 100 characters.
fn title(kind: MessageKind, rule_name: &str) -> String {
    let name: String = rule_name.chars().take(100).collect();
    match kind {
        MessageKind::Fired => format!("Firing: {name}"),
        MessageKind::Resolved => format!("Resolved: {name}"),
        MessageKind::NoData => format!("No data: {name}"),
        MessageKind::Reminder => format!("Still firing: {name}"),
        MessageKind::Test => format!("Test notification from {PRODUCT}"),
    }
}

/// The state word of the email subject and body.
fn state_word(kind: MessageKind) -> &'static str {
    match kind {
        MessageKind::Fired => "Firing",
        MessageKind::Resolved => "Resolved",
        MessageKind::NoData => "No data",
        MessageKind::Reminder => "Still firing",
        MessageKind::Test => "Test",
    }
}

/// Escapes text for Slack's `mrkdwn`. `&` goes first.
fn slack_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Slack rejects button URLs longer than this many characters.
pub(crate) const SLACK_URL_MAX: usize = 3000;

/// The "View traces" URL for Slack. A URL over [`SLACK_URL_MAX`] loses its `filter` and keeps
/// `from` and `to`; a URL that does not parse falls back to the rule link.
fn slack_traces_url(notice: &Notice) -> String {
    if notice.traces_url.len() <= SLACK_URL_MAX {
        return notice.traces_url.clone();
    }
    let Ok(mut url) = url::Url::parse(&notice.traces_url) else {
        return notice.rule_url.clone();
    };
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(key, _)| key == "from" || key == "to")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    url.query_pairs_mut().clear().extend_pairs(kept);
    let short: String = url.into();
    // A very long public URL or slug can still be over the cap: use the rule page.
    if short.len() > SLACK_URL_MAX {
        return notice.rule_url.clone();
    }
    short
}

/// The Slack Block Kit body (D-K-8). The sender serializes it.
pub(crate) fn slack_body(notice: &Notice) -> serde_json::Value {
    let traces_url = slack_traces_url(notice);
    let value = value_text(notice.metric, notice.value);
    let threshold = value_text(notice.metric, Some(notice.threshold));
    let symbol = slack_escape(notice.comparator.symbol());
    let project = slack_escape(&notice.project_name);
    let count = notice
        .count
        .map_or_else(|| "—".to_owned(), |count| count.to_string());
    let text = format!(
        "{} — {value} ({symbol} {threshold}) over {} in {project}",
        // Cut the raw name, then escape, so an entity is never split.
        slack_escape(&title(notice.kind, &notice.rule_name)),
        notice.window.as_str(),
    );
    serde_json::json!({
        "text": text,
        "blocks": [
            {"type": "header", "text": {"type": "plain_text", "text": title(notice.kind, &notice.rule_name)}},
            {"type": "section", "fields": [
                {"type": "mrkdwn", "text": format!("*Project*\n{project}")},
                {"type": "mrkdwn", "text": format!("*Metric*\n{}", notice.metric.as_str())},
                {"type": "mrkdwn", "text": format!("*Value*\n{value}")},
                {"type": "mrkdwn", "text": format!("*Threshold*\n{symbol} {threshold}")},
                {"type": "mrkdwn", "text": format!("*Window*\n{}", notice.window.as_str())},
                {"type": "mrkdwn", "text": format!("*Samples*\n{count}")},
            ]},
            {"type": "actions", "elements": [
                {"type": "button", "text": {"type": "plain_text", "text": "View rule"}, "url": notice.rule_url},
                {"type": "button", "text": {"type": "plain_text", "text": "View traces"}, "url": traces_url},
            ]},
        ],
    })
}

/// The webhook body (spec 07 as amended by D-K-8).
#[derive(Debug, Serialize)]
pub(crate) struct WebhookPayload<'a> {
    #[serde(rename = "type")]
    pub(crate) kind: &'static str,
    pub(crate) event_id: Uuid,
    pub(crate) rule: WebhookRule<'a>,
    pub(crate) project: WebhookProject<'a>,
    pub(crate) value: Option<f64>,
    pub(crate) count: Option<i64>,
    pub(crate) threshold: f64,
    pub(crate) comparator: Comparator,
    pub(crate) window: AlertWindow,
    #[serde(with = "time_serde")]
    pub(crate) window_from: i64,
    #[serde(with = "time_serde")]
    pub(crate) window_to: i64,
    #[serde(with = "time_serde")]
    pub(crate) occurred_at: i64,
    pub(crate) url: &'a str,
    pub(crate) rule_url: &'a str,
}

#[derive(Debug, Serialize)]
pub(crate) struct WebhookRule<'a> {
    pub(crate) id: Uuid,
    pub(crate) name: &'a str,
    pub(crate) metric: AlertMetric,
}

#[derive(Debug, Serialize)]
pub(crate) struct WebhookProject<'a> {
    pub(crate) id: ProjectId,
    pub(crate) name: &'a str,
}

/// The signed webhook body. The payload holds strings, numbers and UUIDs only, so serializing
/// it cannot fail.
pub(crate) fn webhook_body(notice: &Notice) -> Vec<u8> {
    let kind = match notice.kind {
        MessageKind::Fired => "alert.fired",
        MessageKind::Resolved => "alert.resolved",
        MessageKind::NoData => "alert.no_data",
        MessageKind::Reminder => "alert.reminder",
        MessageKind::Test => "alert.test",
    };
    let payload = WebhookPayload {
        kind,
        event_id: notice.event_id,
        rule: WebhookRule {
            id: notice.rule_id,
            name: &notice.rule_name,
            metric: notice.metric,
        },
        project: WebhookProject {
            id: notice.project_id,
            name: &notice.project_name,
        },
        value: notice.value,
        count: notice.count,
        threshold: notice.threshold,
        comparator: notice.comparator,
        window: notice.window,
        window_from: notice.window_from,
        window_to: notice.window_to,
        occurred_at: notice.occurred_at,
        url: &notice.traces_url,
        rule_url: &notice.rule_url,
    };
    serde_json::to_vec(&payload).unwrap_or_default()
}

/// The email for `notice`.
pub(crate) fn email(notice: &Notice) -> Email {
    Email::Alert {
        state: state_word(notice.kind),
        rule_name: notice.rule_name.clone(),
        project_name: notice.project_name.clone(),
        value: value_text(notice.metric, notice.value),
        threshold: value_text(notice.metric, Some(notice.threshold)),
        window: notice.window.as_str(),
        rule_url: notice.rule_url.clone(),
        traces_url: notice.traces_url.clone(),
    }
}

/// Everything an issue notification shows (D-IC-21). Never log it: it holds titles and names.
#[derive(Clone, PartialEq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct IssuesNotice {
    pub(crate) notification_id: Uuid,
    pub(crate) source_id: Uuid,
    pub(crate) source_name: String,
    pub(crate) project_id: ProjectId,
    pub(crate) project_name: String,
    pub(crate) items: Vec<DigestItem>,
    pub(crate) more: u64,
    /// When the notification was recorded.
    pub(crate) occurred_at: i64,
    /// `<public_url>/<org_slug>/<project_slug>/issues?source=<source id>`.
    pub(crate) issues_url: String,
    /// The issue page URL of each item, in the order of `items`.
    pub(crate) issue_urls: Vec<String>,
}

impl fmt::Debug for IssuesNotice {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IssuesNotice")
            .field("items", &self.items.len())
            .field("more", &self.more)
            .finish()
    }
}

/// `<public_url>/<org_slug>/<project_slug>/issues/<issue_id>`.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn issue_url(
    public_url: &PublicUrl,
    org_slug: &str,
    project_slug: &str,
    issue_id: Uuid,
) -> String {
    public_url.join(&format!("/{org_slug}/{project_slug}/issues/{issue_id}"))
}

/// `<public_url>/<org_slug>/<project_slug>/issues?source=<source_id>`.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn issues_url(
    public_url: &PublicUrl,
    org_slug: &str,
    project_slug: &str,
    source_id: Uuid,
) -> String {
    public_url.join(&format!(
        "/{org_slug}/{project_slug}/issues?source={source_id}"
    ))
}

/// `new` for a created issue, `regressed` for a regressed one.
#[cfg_attr(not(test), allow(dead_code))]
fn kind_word(kind: DigestKind) -> &'static str {
    match kind {
        DigestKind::Created => "new",
        DigestKind::Regressed => "regressed",
    }
}

/// An issue title cut to 100 characters (Q7).
#[cfg_attr(not(test), allow(dead_code))]
fn cut_title(title: &str) -> String {
    title.chars().take(100).collect()
}

/// `items.len() + more`: the number of new or regressed issues in the notification.
#[cfg_attr(not(test), allow(dead_code))]
fn total(notice: &IssuesNotice) -> u64 {
    u64::try_from(notice.items.len())
        .unwrap_or(u64::MAX)
        .saturating_add(notice.more)
}

/// The Slack Block Kit body of an issue notification. The sender serializes it. `issue_urls`
/// is read in step with `items`.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn issues_slack_body(notice: &IssuesNotice) -> serde_json::Value {
    let total = total(notice);
    // Slack limits a header to 150 characters. The header is plain text: it is not escaped.
    let header: String = format!(
        "{total} new or regressed {} in {}",
        issues_noun(total),
        notice.source_name
    )
    .chars()
    .take(150)
    .collect();
    let text = slack_escape(&header);
    let mut blocks =
        vec![serde_json::json!({"type": "header", "text": {"type": "plain_text", "text": header}})];
    for (item, url) in notice.items.iter().zip(&notice.issue_urls) {
        // Cut the raw title, then escape, so an entity is never split. `|` would end the link
        // text early, so it becomes `¦`.
        let title = slack_escape(&cut_title(&item.title)).replace('|', "¦");
        blocks.push(serde_json::json!({"type": "section", "text": {
            "type": "mrkdwn",
            "text": format!("• *{}* <{url}|{title}>", kind_word(item.kind)),
        }}));
    }
    if notice.more > 0 {
        blocks.push(serde_json::json!({"type": "context", "elements": [
            {"type": "mrkdwn", "text": format!("and {} more", notice.more)},
        ]}));
    }
    blocks.push(serde_json::json!({"type": "actions", "elements": [
        {"type": "button", "text": {"type": "plain_text", "text": "View issues"}, "url": notice.issues_url},
    ]}));
    serde_json::json!({"text": text, "blocks": blocks})
}

/// The `issues.detected` webhook body (D-IC-21, Q1). Its items hold `issue_id`, `kind` and the
/// whole `title`, and no URL.
#[derive(Debug, Serialize)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct IssuesPayload<'a> {
    #[serde(rename = "type")]
    pub(crate) kind: &'static str,
    pub(crate) notification_id: Uuid,
    #[serde(with = "time_serde")]
    pub(crate) occurred_at: i64,
    pub(crate) source_id: Uuid,
    pub(crate) source_name: &'a str,
    pub(crate) project: WebhookProject<'a>,
    pub(crate) items: &'a [DigestItem],
    pub(crate) more: u64,
    pub(crate) issues_url: &'a str,
}

/// The signed `issues.detected` webhook body. The payload holds strings, numbers and UUIDs
/// only, so serializing it cannot fail.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn issues_webhook_body(notice: &IssuesNotice) -> Vec<u8> {
    let payload = IssuesPayload {
        kind: "issues.detected",
        notification_id: notice.notification_id,
        occurred_at: notice.occurred_at,
        source_id: notice.source_id,
        source_name: &notice.source_name,
        project: WebhookProject {
            id: notice.project_id,
            name: &notice.project_name,
        },
        items: &notice.items,
        more: notice.more,
        issues_url: &notice.issues_url,
    };
    serde_json::to_vec(&payload).unwrap_or_default()
}

/// The email for an issue notification. Titles are cut to 100 characters; at most
/// [`DIGEST_ITEMS`] lines.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn issues_email(notice: &IssuesNotice) -> Email {
    let items = notice
        .items
        .iter()
        .zip(&notice.issue_urls)
        .take(DIGEST_ITEMS)
        .map(|(item, url)| IssueLine {
            kind: kind_word(item.kind),
            title: cut_title(&item.title),
            url: url.clone(),
        })
        .collect();
    Email::Issues {
        source_name: notice.source_name.clone(),
        project_name: notice.project_name.clone(),
        total: total(notice),
        items,
        more: notice.more,
        issues_url: notice.issues_url.clone(),
    }
}

/// A sample notice: error_rate, gt 0.05, 5m, value 0.125, count 48, window [now − 5 min, now).
#[cfg(test)]
pub(crate) fn sample_notice(kind: MessageKind, now: i64) -> Notice {
    Notice {
        kind,
        event_id: Uuid::now_v7(),
        rule_id: Uuid::now_v7(),
        rule_name: "High errors".to_owned(),
        metric: AlertMetric::ErrorRate,
        comparator: Comparator::Gt,
        threshold: 0.05,
        window: AlertWindow::M5,
        value: Some(0.125),
        count: Some(48),
        window_from: now - 300_000_000_000,
        window_to: now,
        occurred_at: now,
        project_id: "01900000-0000-7000-8000-000000000001"
            .parse()
            .expect("a project id"),
        project_name: "Support bot".to_owned(),
        rule_url: "https://obs.example.com/acme/support-bot/alerts/r".to_owned(),
        traces_url: "https://obs.example.com/acme/support-bot/traces?from=a&to=b".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use crate::email::Mailer;

    use super::*;

    fn public() -> PublicUrl {
        serde_json::from_str("\"https://obs.example.com\"").expect("url")
    }

    #[test]
    fn links() {
        let id = Uuid::parse_str("01900000-0000-7000-8000-00000000000a").expect("uuid");
        assert_eq!(
            rule_url(&public(), "acme", "bot", id),
            format!("https://obs.example.com/acme/bot/alerts/{id}")
        );
        let url = traces_url(
            &public(),
            "acme",
            "bot",
            0,
            300_000_000_000,
            Some("environment = 'prod' & x"),
        );
        assert!(
            url.contains("filter=environment+%3D+%27prod%27+%26+x"),
            "{url}"
        );
        assert!(
            url.contains("from=1970-01-01T00%3A00%3A00.000000000Z"),
            "{url}"
        );
        assert!(
            url.contains("to=1970-01-01T00%3A05%3A00.000000000Z"),
            "{url}"
        );
    }

    #[test]
    fn value_texts() {
        assert_eq!(value_text(AlertMetric::ErrorRate, Some(0.125)), "12.5%");
        assert_eq!(value_text(AlertMetric::Cost, Some(3.754)), "$3.75");
        assert_eq!(value_text(AlertMetric::LatencyP95, Some(820.4)), "820 ms");
        assert_eq!(value_text(AlertMetric::Score, Some(0.7)), "0.700");
        assert_eq!(value_text(AlertMetric::Score, None), "no data");
        assert_eq!(value_text(AlertMetric::IssueOccurrences, Some(12.0)), "12");
        assert_eq!(value_text(AlertMetric::IssueOccurrences, Some(0.0)), "0");
        assert_eq!(value_text(AlertMetric::IssueOccurrences, None), "no data");
    }

    #[test]
    fn slack_body_shape() {
        let mut notice = sample_notice(MessageKind::Fired, 1_000_000_000_000);
        notice.rule_name = "<b>&".to_owned();
        let body = slack_body(&notice);
        assert_eq!(body["blocks"][0]["text"]["text"], "Firing: <b>&");
        assert_eq!(
            body["text"],
            "Firing: &lt;b&gt;&amp; — 12.5% (&gt; 5.0%) over 5m in Support bot"
        );
        let fields = body["blocks"][1]["fields"].as_array().expect("fields");
        assert_eq!(fields.len(), 6);
        assert_eq!(fields[3]["text"], "*Threshold*\n&gt; 5.0%");
        assert_eq!(fields[5]["text"], "*Samples*\n48");
        let buttons = body["blocks"][2]["elements"].as_array().expect("buttons");
        assert_eq!(buttons[0]["url"], notice.rule_url.as_str());
        assert_eq!(buttons[1]["url"], notice.traces_url.as_str());

        notice.comparator = Comparator::Lte;
        let body = slack_body(&notice);
        let text = body["blocks"][1]["fields"][3]["text"]
            .as_str()
            .expect("text");
        assert!(text.contains("&lt;="), "{text}");
    }

    #[test]
    fn slack_traces_link_drops_a_long_filter() {
        let mut notice = sample_notice(MessageKind::Fired, 1_000_000_000_000);
        let filter = "a".repeat(4000);
        notice.traces_url = traces_url(&public(), "acme", "bot", 0, 300_000_000_000, Some(&filter));
        assert!(notice.traces_url.len() > 4000);

        let body = slack_body(&notice);
        let button = body["blocks"][2]["elements"][1]["url"]
            .as_str()
            .expect("url");
        assert!(button.len() <= SLACK_URL_MAX, "{} characters", button.len());
        let parsed = url::Url::parse(button).expect("a URL");
        let keys: Vec<String> = parsed
            .query_pairs()
            .map(|(key, _)| key.into_owned())
            .collect();
        assert_eq!(keys, ["from", "to"]);
        assert!(button.contains("from=1970-01-01T00%3A00%3A00.000000000Z"));
        assert!(button.contains("to=1970-01-01T00%3A05%3A00.000000000Z"));

        // The webhook keeps the full URL, filter included.
        let webhook: Value = serde_json::from_slice(&webhook_body(&notice)).expect("json");
        assert_eq!(webhook["url"], notice.traces_url.as_str());
        assert!(
            webhook["url"]
                .as_str()
                .is_some_and(|url| url.contains("filter="))
        );
    }

    #[test]
    fn slack_traces_link_keeps_a_short_url() {
        let notice = sample_notice(MessageKind::Fired, 1_000_000_000_000);
        let body = slack_body(&notice);
        assert_eq!(
            body["blocks"][2]["elements"][1]["url"],
            notice.traces_url.as_str()
        );
    }

    #[test]
    fn no_data_message() {
        let mut notice = sample_notice(MessageKind::NoData, 1_000_000_000_000);
        notice.value = None;
        notice.count = Some(0);
        let body = slack_body(&notice);
        assert_eq!(body["blocks"][0]["text"]["text"], "No data: High errors");
        assert_eq!(
            body["text"],
            "No data: High errors — no data (&gt; 5.0%) over 5m in Support bot"
        );
        let webhook: Value = serde_json::from_slice(&webhook_body(&notice)).expect("json");
        assert_eq!(webhook["type"], "alert.no_data");
        let Email::Alert { state, value, .. } = email(&notice) else {
            panic!("an alert email");
        };
        assert_eq!((state, value.as_str()), ("No data", "no data"));
    }

    /// `count` items, alternately created and regressed, plus `more` further issues.
    fn sample_issues_notice(count: usize, more: u64) -> IssuesNotice {
        let items: Vec<DigestItem> = (0..count)
            .map(|index| DigestItem {
                issue_id: Uuid::now_v7(),
                kind: if index % 2 == 0 {
                    DigestKind::Created
                } else {
                    DigestKind::Regressed
                },
                title: format!("Timeout in tool {index}"),
            })
            .collect();
        let issue_urls = items
            .iter()
            .map(|item| {
                format!(
                    "https://obs.example.com/acme/support-bot/issues/{}",
                    item.issue_id
                )
            })
            .collect();
        IssuesNotice {
            notification_id: Uuid::now_v7(),
            source_id: Uuid::now_v7(),
            source_name: "Errors".to_owned(),
            project_id: "01900000-0000-7000-8000-000000000001"
                .parse()
                .expect("a project id"),
            project_name: "Support bot".to_owned(),
            items,
            more,
            occurred_at: 1_000_000_000_000,
            issues_url: "https://obs.example.com/acme/support-bot/issues?source=s".to_owned(),
            issue_urls,
        }
    }

    fn issues_blocks(notice: &IssuesNotice) -> Vec<Value> {
        let body = issues_slack_body(notice);
        body["blocks"].as_array().expect("blocks").clone()
    }

    #[test]
    fn issue_urls_are_built_from_the_public_url() {
        let id = Uuid::parse_str("01900000-0000-7000-8000-00000000000a").expect("uuid");
        assert_eq!(
            issue_url(&public(), "acme", "bot", id),
            format!("https://obs.example.com/acme/bot/issues/{id}")
        );
        assert_eq!(
            issues_url(&public(), "acme", "bot", id),
            format!("https://obs.example.com/acme/bot/issues?source={id}")
        );
    }

    #[test]
    fn issues_webhook_body_has_the_documented_shape() {
        let notice = sample_issues_notice(2, 3);
        let body: Value = serde_json::from_slice(&issues_webhook_body(&notice)).expect("json");
        let mut keys: Vec<&str> = body
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "issues_url",
                "items",
                "more",
                "notification_id",
                "occurred_at",
                "project",
                "source_id",
                "source_name",
                "type",
            ]
        );
        assert_eq!(body["type"], "issues.detected");
        assert_eq!(body["notification_id"], notice.notification_id.to_string());
        assert_eq!(body["source_id"], notice.source_id.to_string());
        assert_eq!(body["source_name"], "Errors");
        assert_eq!(body["project"]["name"], "Support bot");
        assert_eq!(
            body["project"]["id"],
            serde_json::to_value(notice.project_id).expect("a value")
        );
        assert_eq!(body["more"], 3);
        assert_eq!(body["issues_url"], notice.issues_url.as_str());
        assert!(
            body["occurred_at"]
                .as_str()
                .is_some_and(|text| text.contains('T') && text.ends_with('Z'))
        );
        assert_eq!(body["items"][0]["kind"], "created");
        assert_eq!(body["items"][1]["kind"], "regressed");
        assert_eq!(
            body["items"][0]["issue_id"],
            notice.items[0].issue_id.to_string()
        );
        assert_eq!(body["items"][0]["title"], "Timeout in tool 0");
        let mut item_keys: Vec<&str> = body["items"][0]
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        item_keys.sort_unstable();
        assert_eq!(item_keys, ["issue_id", "kind", "title"]);
    }

    #[test]
    fn issues_webhook_body_keeps_the_whole_title() {
        let mut notice = sample_issues_notice(1, 0);
        let title = "t".repeat(300);
        notice.items[0].title = title.clone();
        let body: Value = serde_json::from_slice(&issues_webhook_body(&notice)).expect("json");
        assert_eq!(body["items"][0]["title"], title.as_str());
    }

    #[test]
    fn issues_slack_body_singular() {
        let notice = sample_issues_notice(1, 0);
        let body = issues_slack_body(&notice);
        assert_eq!(
            body["blocks"][0]["text"]["text"],
            "1 new or regressed issue in Errors"
        );
        assert_eq!(body["text"], "1 new or regressed issue in Errors");
    }

    #[test]
    fn issues_slack_body_header_and_lines() {
        let notice = sample_issues_notice(3, 0);
        let body = issues_slack_body(&notice);
        assert_eq!(body["text"], "3 new or regressed issues in Errors");
        let blocks = issues_blocks(&notice);
        assert_eq!(blocks.len(), 5, "header, three items, actions");
        assert_eq!(
            blocks[0]["text"]["text"],
            "3 new or regressed issues in Errors"
        );
        assert_eq!(
            blocks[1]["text"]["text"],
            format!("• *new* <{}|Timeout in tool 0>", notice.issue_urls[0])
        );
        assert_eq!(
            blocks[2]["text"]["text"],
            format!("• *regressed* <{}|Timeout in tool 1>", notice.issue_urls[1])
        );
        assert_eq!(blocks[3]["type"], "section");
        assert!(blocks.iter().all(|block| block["type"] != "context"));
        assert_eq!(blocks[4]["type"], "actions");
    }

    #[test]
    fn issues_slack_body_with_more() {
        let notice = sample_issues_notice(20, 5);
        let body = issues_slack_body(&notice);
        assert_eq!(
            body["blocks"][0]["text"]["text"],
            "25 new or regressed issues in Errors"
        );
        let blocks = issues_blocks(&notice);
        assert_eq!(blocks.len(), 23, "header, twenty items, context, actions");
        assert_eq!(blocks[21]["type"], "context");
        assert_eq!(blocks[21]["elements"][0]["text"], "and 5 more");
        assert_eq!(blocks[22]["type"], "actions");
        assert_eq!(blocks[22]["elements"][0]["text"]["text"], "View issues");
        assert_eq!(blocks[22]["elements"][0]["url"], notice.issues_url.as_str());
    }

    #[test]
    fn issues_slack_body_cuts_and_escapes_titles() {
        let mut notice = sample_issues_notice(1, 0);
        notice.source_name = "s".repeat(200);
        notice.items[0].title = format!("<b>&|{}", "x".repeat(300));
        let blocks = issues_blocks(&notice);
        let header = blocks[0]["text"]["text"].as_str().expect("a header");
        assert_eq!(header.chars().count(), 150);
        let line = blocks[1]["text"]["text"].as_str().expect("a line");
        let expected = format!(
            "• *new* <{}|&lt;b&gt;&amp;¦{}>",
            notice.issue_urls[0],
            "x".repeat(95)
        );
        assert_eq!(line, expected);
    }

    #[test]
    fn issues_email_subject_and_body() {
        let mailer = Mailer::stub("cetra <noreply@example.com>", Default::default());
        let one = mailer
            .render(&issues_email(&sample_issues_notice(1, 0)))
            .expect("renders");
        assert_eq!(
            one.subject,
            format!("[{PRODUCT}] 1 new or regressed issue: Errors")
        );
        assert!(
            one.text.contains("1 new or regressed issue in Errors"),
            "{}",
            one.text
        );
        assert!(
            one.html.contains("1 new or regressed issue in Errors"),
            "{}",
            one.html
        );
        assert!(!one.html.contains("1 new or regressed issues"));
        assert!(!one.text.contains("1 new or regressed issues"));

        let three = mailer
            .render(&issues_email(&sample_issues_notice(3, 0)))
            .expect("renders");
        assert_eq!(
            three.subject,
            format!("[{PRODUCT}] 3 new or regressed issues: Errors")
        );

        for (count, more) in [(0, 0), (0, 4), (20, 0), (20, 5)] {
            let rendered = mailer
                .render(&issues_email(&sample_issues_notice(count, more)))
                .expect("renders");
            assert!(rendered.text.contains("View the issues"));
            assert!(rendered.html.contains("View the issues"));
        }
    }

    #[test]
    fn issues_email_html_escapes_titles() {
        let mailer = Mailer::stub("cetra <noreply@example.com>", Default::default());
        let mut notice = sample_issues_notice(1, 0);
        notice.items[0].title = "<script>alert(1)</script>".to_owned();
        let rendered = mailer.render(&issues_email(&notice)).expect("renders");
        assert!(
            !rendered.html.contains("<script>alert"),
            "{}",
            rendered.html
        );
        assert!(rendered.html.contains("&lt;script&gt;"));
        assert!(rendered.text.contains("<script>alert(1)</script>"));
    }

    #[test]
    fn notice_debug_hides_titles() {
        let notice = sample_issues_notice(2, 1);
        let debug = format!("{notice:?}");
        assert_eq!(debug, "IssuesNotice { items: 2, more: 1 }");
        assert!(!debug.contains("Timeout"));
        assert!(!debug.contains("Errors"));
    }

    #[test]
    fn webhook_body_shape() {
        let mut notice = sample_notice(MessageKind::Resolved, 1_000_000_000_000);
        notice.value = None;
        notice.count = None;
        let body: Value = serde_json::from_slice(&webhook_body(&notice)).expect("json");
        assert_eq!(body["type"], "alert.resolved");
        assert_eq!(body["event_id"], notice.event_id.to_string());
        assert_eq!(body["rule"]["metric"], "error_rate");
        assert_eq!(body["comparator"], "gt");
        assert_eq!(body["window"], "5m");
        assert!(body.get("value").is_some_and(Value::is_null));
        assert!(body.get("count").is_some_and(Value::is_null));
        for key in ["window_from", "window_to", "occurred_at"] {
            assert!(
                body[key]
                    .as_str()
                    .is_some_and(|text| text.contains('T') && text.ends_with('Z')),
                "{key}"
            );
        }
    }
}
