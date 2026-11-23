//! Product analytics (track TL M6, D-TL-14, D-TL-15): one typed catalog of product events,
//! recorded once wherever an action happens (web UI, API, CLI, SDK, MCP), feeding two sinks:
//! per-event feature counters for the heartbeat's `usage` buckets, and per-user events to the
//! operator's PostHog when `[analytics]` is set.

pub(crate) mod actor;
pub(crate) mod catalog;
