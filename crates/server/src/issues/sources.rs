//! The error sources of every project, as the sweeper reads them (D-IC-2). A project with an
//! enabled error source is tracked even without online rules.

use std::collections::HashMap;

use cetra_core::ids::{OrgId, ProjectId};

use crate::eval::trigger::timeout_ns;
use crate::http::AppState;
use crate::meta::MetaError;
use crate::meta::issues::{self, SweepSource};

/// Error sources of one project, as the sweeper sees them.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProjectSources {
    pub(crate) org: OrgId,
    pub(crate) timeout_ns: i64,
    pub(crate) sources: Vec<SweepSource>,
}

/// Reads the enabled error sources of active projects and groups them by project. Each
/// project's sources are in id order, and its timeout is the same as the online rules' (D-I-8).
///
/// # Errors
///
/// [`MetaError`] when the read fails.
pub(crate) async fn refresh(
    state: &AppState,
) -> Result<HashMap<ProjectId, ProjectSources>, MetaError> {
    let swept = issues::sweep_sources(state.meta.reader()).await?;
    let mut projects: HashMap<ProjectId, ProjectSources> = HashMap::new();
    for source in swept {
        let org = source.org_id;
        let timeout = timeout_ns(source.completion_timeout_s);
        projects
            .entry(source.project_id)
            .or_insert_with(|| ProjectSources {
                org,
                timeout_ns: timeout,
                sources: Vec::new(),
            })
            .sources
            .push(source);
    }
    Ok(projects)
}
