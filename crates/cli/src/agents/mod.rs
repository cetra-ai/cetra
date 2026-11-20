//! `cetra agents`: connects coding agents (Claude Code, Codex, Gemini CLI, Copilot) to cetra
//! (track CA, M4). `hook` posts a session's repository, branch and person; `agents.toml` holds
//! the server, key and person it posts with.

pub(crate) mod hook;

use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::credentials::write_private_file;

/// The coding agents `cetra agents` configures (D-CA-1). Wire names are D-CA-4's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Agent {
    #[value(name = "claude-code")]
    ClaudeCode,
    #[value(name = "codex")]
    Codex,
    #[value(name = "gemini-cli")]
    GeminiCli,
    #[value(name = "copilot")]
    Copilot,
}

impl Agent {
    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::GeminiCli => "gemini-cli",
            Self::Copilot => "copilot",
        }
    }
}

/// `<config dir>/cetra/agents.toml`: what the hook needs. Debug hides the key.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AgentsFile {
    pub(crate) url: String,
    pub(crate) key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) user: Option<String>,
}

impl fmt::Debug for AgentsFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentsFile")
            .field("url", &self.url)
            .field("key", &"***")
            .field("user", &self.user.as_ref().map(|_| "***"))
            .finish()
    }
}

/// The `cetra agents` subcommands. A2 adds `setup` and `remove`.
#[derive(Debug, clap::Subcommand)]
pub(crate) enum AgentsCommand {
    /// Run by an agent's hook: send the session's repository, branch and person to cetra.
    #[command(hide = true)]
    Hook { agent: Agent },
}

/// Runs an `agents` subcommand. `out` receives results and `err` notes. The hook never
/// returns an error.
pub(crate) async fn run(
    command: &AgentsCommand,
    paths: &Paths,
    stdin: impl Read + Send + 'static,
    _out: &mut dyn Write, // Written by `setup` and `remove` (A2).
    err: &mut dyn Write,
) -> anyhow::Result<()> {
    match command {
        AgentsCommand::Hook { agent } => {
            hook::hook(*agent, paths, stdin, err).await;
            Ok(())
        }
    }
}

impl AgentsFile {
    /// `None` when the file is missing; a file that does not parse is an error that never
    /// shows its contents.
    pub(crate) fn load(path: &Path) -> anyhow::Result<Option<AgentsFile>> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(_) => anyhow::bail!("reading agents.toml"),
        };
        toml::from_str(&text)
            .map(Some)
            .map_err(|_| anyhow::anyhow!("agents.toml is invalid"))
    }

    /// Written by `setup` (A2); only tests call it until then.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn save(&self, path: &Path) -> anyhow::Result<()> {
        let text = toml::to_string(self).map_err(|_| anyhow::anyhow!("encoding agents.toml"))?;
        write_private_file(path, text.as_bytes())
    }
}

/// Where `cetra agents` reads and writes. Production values come from `Paths::detect`; tests
/// pass temporary directories.
#[derive(Debug, Clone)]
pub(crate) struct Paths {
    /// Read by `setup` and `remove` (A2).
    #[allow(dead_code)]
    pub(crate) home: PathBuf,
    /// The directory of `credentials.toml`.
    pub(crate) cetra_dir: PathBuf,
    /// Written into the agents' hooks by `setup` (A2).
    #[allow(dead_code)]
    pub(crate) exe: PathBuf,
}

impl Paths {
    pub(crate) fn detect() -> anyhow::Result<Self> {
        let home = dirs::home_dir().context("finding the home directory")?;
        let config = dirs::config_dir().context("finding the config directory")?;
        let exe = std::env::current_exe().context("finding the cetra executable")?;
        Ok(Self {
            home,
            cetra_dir: config.join("cetra"),
            exe,
        })
    }

    pub(crate) fn agents_file(&self) -> PathBuf {
        self.cetra_dir.join("agents.toml")
    }
}
