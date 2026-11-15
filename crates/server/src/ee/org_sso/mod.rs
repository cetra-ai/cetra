// SPDX-License-Identifier: Elastic-2.0
//! Per-org SSO (track P M5, D-P-25, D-P-41): org OIDC providers, verified email domains,
//! enforcement and login discovery. Self-hosted instances; works for any org with `org_sso`.

pub(crate) mod store;
