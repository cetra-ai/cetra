//! Org tokens (D-P-11): validation and the in-memory grants. The window history (D-P-30)
//! lives in `meta::entitlements`.

use llmobs_core::ids::OrgId;
use llmobs_token::{Claims, LimitsClaim, TokenError};
use uuid::Uuid;

use super::{FeatureSet, Limits};

/// What a valid org token grants. Limits are already merged over the instance defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OrgGrant {
    /// The token's `id`.
    pub(crate) token_id: Uuid,
    /// ns.
    pub(crate) issued_at: i64,
    /// ns. Active before this instant.
    pub(crate) expires_at: i64,
    /// ns. In grace before this instant, expired from it on.
    pub(crate) grace_ends_at: i64,
    /// The plan the token names.
    pub(crate) plan: PlanInfo,
    /// The features it lists, `ee` ones included (D-P-11: audience-bound).
    pub(crate) features: FeatureSet,
    /// The limits, with every missing one taken from the instance defaults.
    pub(crate) limits: Limits,
}

/// The plan a token names, for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanInfo {
    /// Plan id, 1 to 64 characters.
    pub(crate) id: String,
    /// Plan name, 1 to 64 characters.
    pub(crate) name: String,
    /// Add-on names, as the token lists them.
    pub(crate) add_ons: Vec<String>,
}

/// Why an org token was refused. Never quotes the token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum OrgTokenError {
    /// The token itself was refused, or it is not an org token (`WrongKind`).
    #[error(transparent)]
    Token(#[from] TokenError),
    /// The token's audience is not this cell's `public_url`.
    #[error("the token is for another cell")]
    Audience,
    /// The token is for another org than the one it was sent for.
    #[error("the token is for another org")]
    OrgMismatch,
    /// The dates are out of order, or the plan id or name is not 1 to 64 characters.
    #[error("the token fields are inconsistent")]
    Fields,
}

/// Verifies `token` as an org token for this cell and returns its org and grant.
///
/// Checks, in order: `llmobs_token::verify` (a non-org kind is `Token(WrongKind)`),
/// `audience == public_url` (`Audience`), then `issued_at <= expires_at <= grace_ends_at`
/// and a plan id and name of 1 to 64 characters (`Fields`). Dates against now are the
/// caller's.
pub(crate) fn grant_from_token(
    token: &str,
    public_url: &str,
    defaults: &Limits,
) -> Result<(OrgId, OrgGrant), OrgTokenError> {
    let verified = llmobs_token::verify(token)?;
    let Claims::Org(claims) = verified.claims else {
        return Err(TokenError::WrongKind.into());
    };
    if claims.audience != public_url {
        return Err(OrgTokenError::Audience);
    }
    let ordered =
        claims.issued_at <= claims.expires_at && claims.expires_at <= claims.grace_ends_at;
    if !ordered || !valid_name(&claims.plan.id) || !valid_name(&claims.plan.name) {
        return Err(OrgTokenError::Fields);
    }
    let grant = OrgGrant {
        token_id: claims.id,
        issued_at: claims.issued_at,
        expires_at: claims.expires_at,
        grace_ends_at: claims.grace_ends_at,
        plan: PlanInfo {
            id: claims.plan.id,
            name: claims.plan.name,
            add_ons: claims.plan.add_ons,
        },
        features: FeatureSet::from_names(claims.features.iter().map(String::as_str)),
        limits: merge_limits(&claims.limits, defaults),
    };
    Ok((OrgId::from_uuid(claims.org_id), grant))
}

fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.chars().count())
}

/// Each limit the token omits takes the default; `null` is unlimited (D-P-13).
fn merge_limits(claim: &LimitsClaim, defaults: &Limits) -> Limits {
    Limits {
        units_included: merge(claim.units_included, defaults.units_included),
        units_cap: merge(claim.units_cap, defaults.units_cap),
        ingest_spans_per_sec: merge(claim.ingest_spans_per_sec, defaults.ingest_spans_per_sec),
        ingest_burst: merge(claim.ingest_burst, defaults.ingest_burst),
        data_access_days: merge(claim.data_access_days, defaults.data_access_days),
        max_seats: merge(claim.max_seats, defaults.max_seats),
        max_projects: merge(claim.max_projects, defaults.max_projects),
        max_alert_rules: merge(claim.max_alert_rules, defaults.max_alert_rules),
        max_active_online_rules: merge(
            claim.max_active_online_rules,
            defaults.max_active_online_rules,
        ),
    }
}

fn merge(claim: Option<Option<u64>>, default: Option<u64>) -> Option<u64> {
    claim.unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use llmobs_core::ids::OrgId;
    use llmobs_token::TokenError;
    use serde_json::json;
    use uuid::Uuid;

    use super::{OrgTokenError, grant_from_token};
    use crate::config::PublicUrl;
    use crate::entitlements::Limits;
    use crate::test_support::{control_plane_token, license_key, org_token, sign_token};

    fn origin() -> String {
        PublicUrl::default().origin().to_owned()
    }

    #[test]
    fn grant_merges_limits_over_defaults() {
        let defaults = Limits {
            max_seats: Some(1),
            units_cap: Some(100),
            ..Limits::default()
        };
        let org = OrgId::new_v7();
        let token = org_token(
            org,
            "2025-12-01T00:00:00Z",
            "2099-01-01T00:00:00Z",
            "2099-02-01T00:00:00Z",
            &["project_roles"],
            &json!({"units_cap": null, "data_access_days": 120}),
        );
        let (granted, grant) = grant_from_token(&token, &origin(), &defaults).unwrap();
        assert_eq!(granted, org);
        assert_eq!(grant.limits.units_cap, None);
        assert_eq!(grant.limits.data_access_days, Some(120));
        assert_eq!(grant.limits.max_seats, Some(1));
        assert_eq!(grant.limits.units_included, None);
        assert_eq!(grant.plan.id, "pro");
        assert_eq!(grant.plan.add_ons, vec!["teams".to_owned()]);
    }

    #[test]
    fn grant_refusals() {
        let org = OrgId::new_v7();
        let defaults = Limits::default();
        let refuse = |token: &str| grant_from_token(token, &origin(), &defaults).unwrap_err();

        let foreign = sign_token(&json!({
            "kind": "org",
            "id": Uuid::now_v7().to_string(),
            "issued_at": "2025-12-01T00:00:00Z",
            "expires_at": "2099-01-01T00:00:00Z",
            "grace_ends_at": "2099-02-01T00:00:00Z",
            "audience": "https://elsewhere.example",
            "org_id": org.to_string(),
            "plan": {"id": "pro", "name": "Pro"},
        }));
        assert_eq!(refuse(&foreign), OrgTokenError::Audience);

        assert_eq!(
            refuse(&license_key(&[])),
            OrgTokenError::Token(TokenError::WrongKind)
        );
        assert_eq!(
            refuse(&control_plane_token("orgs")),
            OrgTokenError::Token(TokenError::WrongKind)
        );

        let inverted = org_token(
            org,
            "2025-12-01T00:00:00Z",
            "2099-02-01T00:00:00Z",
            "2099-01-01T00:00:00Z",
            &[],
            &json!({}),
        );
        assert_eq!(refuse(&inverted), OrgTokenError::Fields);

        let nameless = sign_token(&json!({
            "kind": "org",
            "id": Uuid::now_v7().to_string(),
            "issued_at": "2025-12-01T00:00:00Z",
            "expires_at": "2099-01-01T00:00:00Z",
            "grace_ends_at": "2099-02-01T00:00:00Z",
            "audience": origin(),
            "org_id": org.to_string(),
            "plan": {"id": "pro", "name": ""},
        }));
        assert_eq!(refuse(&nameless), OrgTokenError::Fields);

        assert_eq!(
            refuse("garbage"),
            OrgTokenError::Token(TokenError::Malformed)
        );
    }
}
