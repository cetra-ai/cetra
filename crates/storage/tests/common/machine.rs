//! What a benchmark or soak write-up names about the machine (track R M7, R7.1).
//!
//! `findmnt` prints the first row of `lsblk -d` in the old code; that row was a
//! card reader on the workstation, not the disk the data lived on. The disk is
//! now found by walking from the mounted device down to its physical disk.

use std::fmt;
use std::path::Path;
use std::process::Command;

/// Everything a benchmark or soak write-up names about the machine.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct Machine {
    /// `/proc/cpuinfo` "model name".
    pub(crate) cpu: String,
    /// `available_parallelism`.
    pub(crate) threads: usize,
    /// `/proc/meminfo` "MemTotal:".
    pub(crate) ram: String,
    /// `uname -r`.
    pub(crate) kernel: String,
    /// Physical disk under the data directory: "<name> rota=<0|1> <model>".
    pub(crate) disk: String,
    /// `findmnt -n -o FSTYPE,OPTIONS -T <dir>`.
    pub(crate) filesystem: String,
    /// `/run/.containerenv` or `/.dockerenv` exists.
    pub(crate) container: bool,
    /// `git rev-parse HEAD`, 12 characters.
    pub(crate) commit: String,
    /// `git status --porcelain` is not empty.
    pub(crate) dirty: bool,
    /// `rustc -V`.
    pub(crate) rustc: String,
}

/// The machine a run uses, with `dir` as the data directory.
pub(crate) fn machine(dir: &Path) -> Machine {
    let dir_text = dir.display().to_string();
    let source = command(&["findmnt", "-n", "-o", "SOURCE", "-T", &dir_text]);
    let source = strip_subvolume(&source).to_owned();
    let disk = physical_disk(&command(&[
        "lsblk",
        "-s",
        "-n",
        "-o",
        "NAME,ROTA,MODEL,TYPE",
        &source,
    ]))
    .unwrap_or_else(|| "unknown".to_owned());
    let status = command(&["git", "status", "--porcelain"]);
    let commit = command(&["git", "rev-parse", "HEAD"]);
    Machine {
        cpu: read_line("/proc/cpuinfo", "model name"),
        threads: std::thread::available_parallelism().map_or(0, |n| n.get()),
        ram: read_line("/proc/meminfo", "MemTotal:"),
        kernel: command(&["uname", "-r"]),
        disk,
        filesystem: command(&["findmnt", "-n", "-o", "FSTYPE,OPTIONS", "-T", &dir_text]),
        container: Path::new("/run/.containerenv").exists() || Path::new("/.dockerenv").exists(),
        commit: commit[..commit.len().min(12)].to_owned(),
        dirty: !status.is_empty() && status != "unknown",
        rustc: command(&["rustc", "-V"]),
    }
}

impl fmt::Display for Machine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "CPU {}, {} threads, RAM {}, disk [{}], filesystem [{}], kernel {}, container {}, commit {}{}, {}",
            self.cpu,
            self.threads,
            self.ram,
            self.disk,
            self.filesystem,
            self.kernel,
            self.container,
            self.commit,
            if self.dirty { " (dirty)" } else { "" },
            self.rustc,
        )
    }
}

/// "/dev/mapper/luks-x[/home]" → "/dev/mapper/luks-x": findmnt prints a btrfs
/// subvolume in brackets.
pub(crate) fn strip_subvolume(source: &str) -> &str {
    source.split('[').next().unwrap_or(source).trim()
}

/// From `lsblk -s -n -o NAME,ROTA,MODEL,TYPE <source>` output, the first line
/// with TYPE `disk`, as "<name> rota=<r> <model>".
///
/// Names may start with tree characters (`└─`), which are stripped. The model
/// may contain spaces and may be blank, so the line is not split on every
/// whitespace: TYPE is the last token, NAME the first, ROTA the second, and
/// MODEL everything between.
pub(crate) fn physical_disk(lsblk_inverse: &str) -> Option<String> {
    lsblk_inverse.lines().find_map(|line| {
        let line = line.trim_start_matches(|c: char| !c.is_alphanumeric());
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() < 3 || tokens[tokens.len() - 1] != "disk" {
            return None;
        }
        let name = tokens[0];
        let rota = tokens[1];
        let model = tokens[2..tokens.len() - 1].join(" ");
        let model = if model.is_empty() {
            "unknown".to_owned()
        } else {
            model
        };
        Some(format!("{name} rota={rota} {model}"))
    })
}

/// The `prefix` line of a `/proc` file, with its value.
pub(crate) fn read_line(path: &str, prefix: &str) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .find(|line| line.starts_with(prefix))
        .map(|line| {
            line[prefix.len()..]
                .trim()
                .trim_start_matches(':')
                .trim()
                .to_owned()
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Trimmed stdout of a command, or "unknown" when it cannot be run.
pub(crate) fn command(args: &[&str]) -> String {
    Command::new(args[0])
        .args(&args[1..])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Total length of the regular files under `dir`.
pub(crate) fn dir_bytes(dir: &Path) -> u64 {
    fn walk(path: &Path, total: &mut u64) {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                walk(&p, total);
            } else if let Ok(meta) = p.metadata() {
                *total += meta.len();
            }
        }
    }
    let mut total = 0;
    walk(dir, &mut total);
    total
}
