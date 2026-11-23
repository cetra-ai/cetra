// SPDX-License-Identifier: Elastic-2.0
//! SCIM 2.0 provisioning (track EA M5, D-EA-12 to D-EA-19).

pub(crate) mod domains;
pub(crate) mod error;
pub(crate) mod filter;
pub(crate) mod patch;
pub(crate) mod schema;
pub(crate) mod settings;
pub(crate) mod store;
pub(crate) mod users;

use cetra_core::ids::OrgId;

use crate::api::entitlements::Feature;
use crate::http::AppState;
use crate::http::access::RouteDef;
use crate::meta::MetaError;

/// The `/scim/v2` routes and the Provisioning API routes.
pub(crate) fn routes() -> Vec<RouteDef> {
    let mut routes = users::routes();
    routes.extend(settings::routes());
    routes
}

/// D-EA-18: SCIM is on and entitled for `org`. Read outside a write; pass the bool in.
pub(crate) async fn locks(state: &AppState, org: OrgId) -> Result<bool, MetaError> {
    Ok(store::org_scim(state.meta.reader(), org).await?.enabled
        && crate::entitlements::of(state, org).allows(Feature::Scim))
}
