//! Request and response types for the HTTP API.
//!
//! The rules live in `docs/spec/09-api.md` § Conventions and § Response shapes.
//! The generated contract file is `crates/server/openapi.json`.

pub mod account;
pub mod admin;
pub mod agent_context;
pub mod alerts;
pub mod annotation_queues;
pub mod assistant;
pub mod backfills;
pub mod common;
pub mod connections;
pub(crate) mod convert;
pub mod dashboards;
pub mod datasets;
pub mod deletions;
pub mod device;
pub mod entitlements;
pub mod eval_jobs;
pub mod evaluators;
pub mod experiments;
pub mod facets;
pub mod internal;
pub mod invites;
pub mod issues;
pub mod keys;
pub mod members;
pub mod metrics;
pub mod model_prices;
pub mod online_rules;
pub mod openapi;
pub mod org_sso;
pub mod payload;
pub mod playground;
pub mod prompts;
pub mod saved_views;
pub mod schema;
pub mod score_definitions;
pub mod scores;
pub mod scim;
pub mod sessions;
pub mod sql;
pub mod traces;
pub mod usage;
pub mod users;
