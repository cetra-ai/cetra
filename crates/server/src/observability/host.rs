//! Facts about the host for the debug bundle (track TL § 4): operating system, kernel, CPU count,
//! memory and the cgroup memory limit, read from the OS through `sysinfo`. Nothing leaves the
//! process. The memory calls repeat those of `cetra_storage`'s `memory::detected_memory`, which
//! is `pub(crate)` in that crate and so not reachable from here (storage's public API does not
//! change in this track).

use std::num::NonZeroUsize;

use serde::Serialize;
use sysinfo::{
    MemoryRefreshKind, Process, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System,
};

use super::facts::{Deployment, deployment, probe_host};

/// The host facts of the debug bundle. Every value is read from the OS; none comes from user
/// data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct HostFacts {
    /// `std::env::consts::OS`, e.g. `linux`.
    pub(crate) os: &'static str,
    /// The long OS version from `sysinfo`, `None` when it cannot be read.
    pub(crate) os_version: Option<String>,
    /// The kernel release from `sysinfo`, `None` when it cannot be read.
    pub(crate) kernel_release: Option<String>,
    /// `std::env::consts::ARCH`, e.g. `x86_64`.
    pub(crate) arch: &'static str,
    /// The CPUs this process may use (`available_parallelism`).
    pub(crate) cpus: Option<usize>,
    /// The machine's RAM in bytes, `None` when the OS reports nothing.
    pub(crate) total_memory_bytes: Option<u64>,
    /// This process's cgroup memory limit in bytes; `None` off Linux or without a limit.
    pub(crate) container_memory_limit_bytes: Option<u64>,
    /// How the process runs (Kubernetes, Docker, or the binary on a host).
    pub(crate) deployment: Deployment,
}

/// The host facts now.
pub(crate) fn host_facts() -> HostFacts {
    let (total_memory_bytes, container_memory_limit_bytes) = memory();
    HostFacts {
        os: std::env::consts::OS,
        os_version: System::long_os_version().ok(),
        kernel_release: System::kernel_version().ok(),
        arch: std::env::consts::ARCH,
        cpus: std::thread::available_parallelism()
            .ok()
            .map(NonZeroUsize::get),
        total_memory_bytes,
        container_memory_limit_bytes,
        deployment: deployment(probe_host()),
    }
}

/// The machine's RAM and this process's cgroup memory limit (its own cgroup and its
/// ancestors, cgroup v2 or v1; `sysinfo` reports only limits below the RAM). Either is
/// `None` when the OS reports nothing; the limit is always `None` off Linux.
fn memory() -> (Option<u64>, Option<u64>) {
    let Ok(mut system) = System::new_with_specifics(
        RefreshKind::nothing().with_memory(MemoryRefreshKind::nothing().with_ram()),
    ) else {
        return (None, None);
    };
    let total = Some(system.total_memory()).filter(|bytes| *bytes > 0);
    let Ok(pid) = sysinfo::get_current_pid() else {
        return (total, None);
    };
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        false,
        ProcessRefreshKind::nothing(),
    );
    let limit = system
        .process(pid)
        .and_then(Process::cgroup_limits)
        .map(|limits| limits.total_memory)
        .filter(|bytes| *bytes > 0);
    (total, limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_facts_name_this_platform() {
        let facts = host_facts();
        assert_eq!(facts.os, std::env::consts::OS);
        assert_eq!(facts.arch, std::env::consts::ARCH);
        assert!(matches!(facts.cpus, Some(n) if n > 0));
        if cfg!(target_os = "linux") {
            assert!(facts.total_memory_bytes.is_some());
        }
        if let (Some(total), Some(limit)) =
            (facts.total_memory_bytes, facts.container_memory_limit_bytes)
        {
            assert!(limit <= total);
        }
    }

    #[test]
    fn there_is_no_cgroup_limit_off_linux() {
        if !cfg!(target_os = "linux") {
            assert_eq!(memory().1, None);
        }
    }

    #[test]
    fn host_facts_serialize_with_the_contract_keys() {
        let value = serde_json::to_value(host_facts()).expect("host facts serialize");
        let keys: Vec<&str> = value
            .as_object()
            .expect("host facts are an object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "os",
                "os_version",
                "kernel_release",
                "arch",
                "cpus",
                "total_memory_bytes",
                "container_memory_limit_bytes",
                "deployment",
            ]
        );
    }
}
