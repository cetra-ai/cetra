//! Build and host facts shared by the heartbeat, crash reports and the debug bundle (track TL). Every value is fixed by the build or read from a fixed probe; none comes from user data.

use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde::Serialize;

/// When the process started, as far as this crate knows (see [`mark_process_start`]).
static PROCESS_START: OnceLock<Instant> = OnceLock::new();

/// Records now as the process start. The first call wins; later calls change nothing.
/// `serve::bind` calls it first thing; M5 may call it earlier.
pub(crate) fn mark_process_start() {
    let _ = PROCESS_START.get_or_init(Instant::now);
}

/// Time since [`mark_process_start`] (or since this first call, if it never ran). The
/// `uptime` of the heartbeat and of crash reports.
pub(crate) fn process_uptime() -> Duration {
    PROCESS_START.get_or_init(Instant::now).elapsed()
}

/// The workspace version, e.g. `0.1.0`.
pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The target triple of this build, e.g. `x86_64-unknown-linux-musl` (`build.rs`).
pub(crate) const TARGET: &str = env!("BUILD_TARGET");

/// The build profile, by `debug_assertions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Profile {
    /// `debug_assertions` off.
    Release,
    /// `debug_assertions` on.
    Debug,
}

/// This build's profile.
pub(crate) const fn profile() -> Profile {
    if cfg!(debug_assertions) {
        Profile::Debug
    } else {
        Profile::Release
    }
}

/// How the process runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Deployment {
    /// A container (Docker, Podman, Compose).
    Docker,
    /// A Kubernetes pod.
    Kubernetes,
    /// Anything else: the binary on a host.
    Binary,
}

/// What [`deployment`] looks at, read once by [`probe_host`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct HostProbe {
    /// `/.dockerenv` exists (Docker).
    pub(crate) dockerenv: bool,
    /// `/run/.containerenv` exists (Podman).
    pub(crate) containerenv: bool,
    /// `KUBERNETES_SERVICE_HOST` is set and non-empty.
    pub(crate) kubernetes: bool,
}

/// Reads the probe files and variable of [`HostProbe`].
pub(crate) fn probe_host() -> HostProbe {
    HostProbe {
        dockerenv: Path::new("/.dockerenv").exists(),
        containerenv: Path::new("/run/.containerenv").exists(),
        kubernetes: std::env::var_os("KUBERNETES_SERVICE_HOST")
            .is_some_and(|value| !value.is_empty()),
    }
}

/// Kubernetes first (a pod is also a container), then a container, else the binary.
pub(crate) fn deployment(probe: HostProbe) -> Deployment {
    if probe.kubernetes {
        Deployment::Kubernetes
    } else if probe.dockerenv || probe.containerenv {
        Deployment::Docker
    } else {
        Deployment::Binary
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_uptime_grows_from_the_first_mark() {
        mark_process_start();
        let a = process_uptime();
        std::thread::sleep(Duration::from_millis(20));
        let b = process_uptime();
        assert!(b > a);
        // A second mark does not reset the start.
        mark_process_start();
        assert!(process_uptime() >= b);
    }

    #[test]
    fn the_version_is_the_package_version() {
        assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn the_target_is_a_triple() {
        assert!(TARGET.matches('-').count() >= 2);
        assert_ne!(TARGET, "unknown");
    }

    #[test]
    fn a_test_build_is_a_debug_profile() {
        assert_eq!(profile(), Profile::Debug);
    }

    #[test]
    fn kubernetes_wins_over_a_container_file() {
        let probe = HostProbe {
            dockerenv: true,
            containerenv: true,
            kubernetes: true,
        };
        assert_eq!(deployment(probe), Deployment::Kubernetes);
    }

    #[test]
    fn a_docker_file_is_docker() {
        let probe = HostProbe {
            dockerenv: true,
            ..HostProbe::default()
        };
        assert_eq!(deployment(probe), Deployment::Docker);
    }

    #[test]
    fn a_podman_file_is_docker() {
        let probe = HostProbe {
            containerenv: true,
            ..HostProbe::default()
        };
        assert_eq!(deployment(probe), Deployment::Docker);
    }

    #[test]
    fn no_probe_is_binary() {
        assert_eq!(deployment(HostProbe::default()), Deployment::Binary);
    }

    #[test]
    fn profile_and_deployment_serialize_lowercase() {
        assert_eq!(
            serde_json::to_string(&Profile::Release).unwrap(),
            "\"release\""
        );
        assert_eq!(serde_json::to_string(&Profile::Debug).unwrap(), "\"debug\"");
        assert_eq!(
            serde_json::to_string(&Deployment::Docker).unwrap(),
            "\"docker\""
        );
        assert_eq!(
            serde_json::to_string(&Deployment::Kubernetes).unwrap(),
            "\"kubernetes\""
        );
        assert_eq!(
            serde_json::to_string(&Deployment::Binary).unwrap(),
            "\"binary\""
        );
    }
}
