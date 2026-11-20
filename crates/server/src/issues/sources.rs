//! The issue sources of every project, as the sweeper reads them (D-IC-2). A project with an
//! enabled error or topic source is tracked even without online rules.

use std::collections::HashMap;

use cetra_core::ids::{OrgId, ProjectId};

use super::triggers;
use crate::eval::trigger::timeout_ns;
use crate::http::AppState;
use crate::meta::MetaError;
use crate::meta::issues::{self, SourceKind, SourceRow, SweepSource};

/// Issue sources of one project, as the sweeper sees them.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProjectSources {
    pub(crate) org: OrgId,
    pub(crate) timeout_ns: i64,
    /// Enabled error sources, in id order.
    pub(crate) sources: Vec<SweepSource>,
    /// Enabled topic sources, in id order (D-IC-4).
    pub(crate) topics: Vec<SourceRow>,
}

/// Reads the enabled error and topic sources of active projects and groups them by project. Each
/// project's sources are in id order, and its timeout is the same as the online rules' (D-I-8).
/// It also rebuilds the score rules (`triggers::refresh_score_rules`).
///
/// # Errors
///
/// [`MetaError`] when a read fails.
pub(crate) async fn refresh(
    state: &AppState,
) -> Result<HashMap<ProjectId, ProjectSources>, MetaError> {
    let swept = issues::sweep_sources(state.meta.reader()).await?;
    let llm = issues::llm_sources(state.meta.reader()).await?;
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
                topics: Vec::new(),
            })
            .sources
            .push(source);
    }
    for (source, org, completion_timeout_s) in llm {
        if source.kind != SourceKind::Topic {
            continue;
        }
        let timeout = timeout_ns(completion_timeout_s);
        projects
            .entry(source.project_id)
            .or_insert_with(|| ProjectSources {
                org,
                timeout_ns: timeout,
                sources: Vec::new(),
                topics: Vec::new(),
            })
            .topics
            .push(source);
    }
    triggers::refresh_score_rules(state).await?;
    Ok(projects)
}
