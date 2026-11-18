//! When each project last received spans over OTLP (D-C-20). In memory only: unknown after a
//! start until the project's next batch.

use std::collections::HashMap;
use std::sync::Mutex;

use cetra_core::ids::ProjectId;

#[derive(Debug, Default)]
pub(crate) struct LastIngest {
    times: Mutex<HashMap<ProjectId, i64>>,
}

impl LastIngest {
    /// Records a committed batch with spans at `now_ns`; an older time never replaces a newer one.
    pub(crate) fn record(&self, project: ProjectId, now_ns: i64) {
        let mut times = self
            .times
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = times.entry(project).or_insert(now_ns);
        *entry = (*entry).max(now_ns);
    }

    /// The time of the project's last committed batch with spans, if any since the server started.
    pub(crate) fn get(&self, project: ProjectId) -> Option<i64> {
        self.times
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&project)
            .copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_project_has_no_time() {
        assert_eq!(LastIngest::default().get(ProjectId::new_v7()), None);
    }

    #[test]
    fn record_keeps_the_newest_time() {
        let last = LastIngest::default();
        let project = ProjectId::new_v7();
        last.record(project, 10);
        last.record(project, 5);
        assert_eq!(last.get(project), Some(10));
        last.record(project, 20);
        assert_eq!(last.get(project), Some(20));
    }

    #[test]
    fn projects_are_kept_apart() {
        let last = LastIngest::default();
        let first = ProjectId::new_v7();
        let second = ProjectId::new_v7();
        last.record(first, 10);
        assert_eq!(last.get(first), Some(10));
        assert_eq!(last.get(second), None);
    }
}
