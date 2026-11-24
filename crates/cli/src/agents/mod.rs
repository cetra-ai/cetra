//! `cetra agents`: connects coding agents (Claude Code, Codex, Gemini CLI, Copilot, Pi, Qwen Code,
//! OpenClaw, OpenCode) to cetra (track CA, M4; Pi, Qwen Code, OpenClaw and OpenCode in track IN, M7). `setup` writes the agent's user settings and `remove` takes
//! them back; `hook` posts a session's repository, branch and person. `agents.toml` holds the
//! server, key and person the hook posts with.

mod codex;
pub(crate) mod hook;
mod openclaw;
mod opencode;
mod pi;
mod qwen_code;
mod setup;

use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::client::Token;
use crate::credentials::write_private_file;

/// The coding agents `cetra agents` configures (D-CA-1, D-IN-28). Wire names are D-CA-4's.
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
    /// Pi: its extension file is written by setup; no hook (D-IN-28 item 9).
    #[value(name = "pi")]
    Pi,
    /// Qwen Code: a Gemini CLI fork, set up like Gemini CLI (D-IN-28 item 7).
    #[value(name = "qwen-code")]
    QwenCode,
    /// OpenClaw: setup installs its diagnostics plugin and merges its config (D-IN-28 items 5, 6).
    #[value(name = "openclaw")]
    OpenClaw,
    /// OpenCode: cetra's plugin and its config file are written by setup; no hook (D-IN-29).
    #[value(name = "opencode")]
    OpenCode,
}

impl Agent {
    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::GeminiCli => "gemini-cli",
            Self::Copilot => "copilot",
            Self::Pi => "pi",
            Self::QwenCode => "qwen-code",
            Self::OpenClaw => "openclaw",
            Self::OpenCode => "opencode",
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

/// The `cetra agents` subcommands.
#[derive(Debug, clap::Subcommand)]
pub(crate) enum AgentsCommand {
    /// Run by an agent's hook: send the session's repository, branch and person to cetra.
    #[command(hide = true)]
    Hook { agent: Agent },
    /// Configure an agent to send its traces to cetra (writes the agent's user settings).
    Setup(SetupArgs),
    /// Remove what `cetra agents setup` wrote for an agent.
    Remove { agent: Agent },
}

#[derive(Debug, clap::Args)]
pub(crate) struct SetupArgs {
    pub(crate) agent: Agent,
    /// cetra server URL, for example `https://cetra.example.com`.
    #[arg(long)]
    pub(crate) url: String,
    /// Project secret key with the `ingest` scope. A value given as a flag is visible to other local users in the process list; prefer CETRA_INGEST_KEY.
    #[arg(long, env = "CETRA_INGEST_KEY", hide_env_values = true)]
    pub(crate) key: Token,
    /// The person to attribute sessions to (default: `git config --global user.email`).
    #[arg(long)]
    pub(crate) user: Option<String>,
    /// Do not turn on prompt and tool content capture.
    #[arg(long)]
    pub(crate) no_content: bool,
    /// Print the merged settings files (including the key and any other settings in them) instead of writing them.
    #[arg(long)]
    pub(crate) print: bool,
}

/// Runs an `agents` subcommand. `out` receives results and `err` notes. The hook never
/// returns an error.
pub(crate) async fn run(
    command: &AgentsCommand,
    paths: &Paths,
    stdin: impl Read + Send + 'static,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> anyhow::Result<()> {
    match command {
        AgentsCommand::Hook { agent } => {
            hook::hook(*agent, paths, stdin, err).await;
            Ok(())
        }
        AgentsCommand::Setup(args) => setup::setup(args, paths, out),
        AgentsCommand::Remove { agent } => setup::remove(*agent, paths, out),
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

    /// Written with mode 0600 through `write_private_file`.
    pub(crate) fn save(&self, path: &Path) -> anyhow::Result<()> {
        let text = toml::to_string(self).map_err(|_| anyhow::anyhow!("encoding agents.toml"))?;
        write_private_file(path, text.as_bytes())
    }
}

/// Where `cetra agents` reads and writes. Production values come from `Paths::detect`; tests
/// pass temporary directories.
#[derive(Debug, Clone)]
pub(crate) struct Paths {
    /// The home directory: the agents' user settings live under it.
    pub(crate) home: PathBuf,
    /// The directory of `credentials.toml` and `agents.toml`.
    pub(crate) cetra_dir: PathBuf,
    /// The cetra executable, written into the agents' hooks.
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
