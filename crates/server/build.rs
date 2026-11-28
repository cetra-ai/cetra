//! Exports the target triple the crate is built for as `BUILD_TARGET`, for the heartbeat and
//! crash reports (track TL, `observability::facts::TARGET`). A build script reports to Cargo on
//! stdout, so the workspace's print lint does not apply here.
#![allow(clippy::print_stdout)]

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // Cargo always sets TARGET for build scripts; "unknown" keeps a hand-run build compiling.
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_owned());
    println!("cargo:rustc-env=BUILD_TARGET={target}");
}
