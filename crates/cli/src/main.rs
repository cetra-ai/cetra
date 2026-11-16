mod client;
mod credentials;
mod health;
mod login;
mod output;
mod projects;
mod query;
#[cfg(test)]
mod test_server;
mod traces;

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use cetra_server::admin::{self, EntitlementsReport, UserSummary};
use cetra_server::admin_socket::{ADMIN_SOCKET, AdminRequest, AdminResponse};
use cetra_server::api::entitlements::OrgLimits;
use cetra_server::backup::BackupMode;
use cetra_server::config::{LogConfig, LogFormat, ServerConfig};
use cetra_server::serve::{self, ServeError};
use clap::{Parser, Subcommand};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing_subscriber::EnvFilter;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Debug, Parser)]
#[command(name = "cetra", version, about = "LLM observability server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start the HTTP server.
    Serve {
        /// Config file. Overrides `CETRA_CONFIG`.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Admin operations on this data directory (users, encryption key, deletions).
    Admin {
        #[command(subcommand)]
        command: AdminCommand,
    },
    /// Print the cetra version.
    Version,
    /// Exit 0 if the server answers `GET <url>/readyz` with 200 (the container healthcheck).
    Health {
        /// Server URL.
        #[arg(long, default_value = health::DEFAULT_URL)]
        url: String,
    },
    /// Save a personal access token for a server.
    ///
    /// The token expires after 90 days. Run `cetra login` again when it does.
    Login {
        /// Server URL, for example `http://127.0.0.1:8080`.
        #[arg(long)]
        url: String,
        /// Print the login URL and do not open a browser.
        #[arg(long)]
        no_browser: bool,
    },
    /// Revoke the saved personal access token.
    Logout {
        /// Server URL. Required when more than one server is saved.
        #[arg(long)]
        url: Option<String>,
    },
    /// Run a SQL query on a project.
    Query(Box<query::QueryArgs>),
    /// List and show traces.
    Traces {
        #[command(subcommand)]
        command: Box<traces::TracesCommand>,
    },
    /// List the projects you can read.
    Projects {
        #[command(subcommand)]
        command: Box<projects::ProjectsCommand>,
    },
}

#[derive(Debug, Subcommand)]
enum AdminCommand {
    /// Create a user.
    CreateUser {
        /// Email address.
        #[arg(long)]
        email: String,
        /// Display name.
        #[arg(long)]
        name: Option<String>,
        /// Read the password from one line of stdin.
        #[arg(long)]
        password_stdin: bool,
        /// Make the user an instance admin.
        #[arg(long)]
        instance_admin: bool,
        /// Config file. Overrides `CETRA_CONFIG`.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Print a password reset URL.
    ResetPassword {
        /// Email address.
        #[arg(long)]
        email: String,
        /// Config file. Overrides `CETRA_CONFIG`.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// List users.
    ListUsers {
        /// `table` or `json`.
        #[arg(long, value_enum, default_value = "table")]
        format: output::ListFormat,
        /// Config file. Overrides `CETRA_CONFIG`.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Re-encrypt stored secrets under a new key. Stop the server first.
    RotateEncryptionKey {
        /// Read the new key (32 bytes, standard base64) from one line of stdin. Default:
        /// generate one, unless the current key comes from `CETRA_ENCRYPTION_KEY`.
        #[arg(long)]
        new_from_stdin: bool,
        /// Config file. Overrides `CETRA_CONFIG`.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Re-open the deletions of another data directory that this one lacks (after a backup restore).
    /// Both `meta.db` files must be at this binary's schema; run `cetra admin migrate` on a restored older backup first.
    ReapplyDeletions {
        /// The data directory whose `meta.db` holds the deletions to re-apply (for example the
        /// directory as it was before the restore).
        #[arg(long)]
        from: PathBuf,
        /// Config file. Overrides `CETRA_CONFIG`.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Back up the running server into DIR (online; the server writes DIR).
    Backup {
        /// Target directory on the server's filesystem: absent or empty, outside the data directory.
        #[arg(long)]
        to: PathBuf,
        /// Metadata only (S3-compatible object storage; objects stay in the bucket).
        #[arg(long)]
        meta_only: bool,
        /// Config file. Overrides `CETRA_CONFIG`.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Delete objects the catalog does not know and report the ones it misses. Stop the server first.
    GcOrphans {
        /// Leave unknown objects younger than this (e.g. `1h`, `30m`).
        #[arg(long, default_value = "1h", value_parser = humantime_duration)]
        min_age: Duration,
        /// Report only; delete nothing.
        #[arg(long)]
        dry_run: bool,
        /// Required in S3 mode without `--dry-run`: deleting orphans there rolls the live bucket back.
        #[arg(long)]
        rollback_bucket: bool,
        /// Config file. Overrides `CETRA_CONFIG`.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Migrate `meta.db` and `catalog.db` to this version's schema, then exit. Stop the server first.
    Migrate {
        /// Config file. Overrides `CETRA_CONFIG`.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Org entitlements.
    Entitlements {
        #[command(subcommand)]
        command: EntitlementsCommand,
    },
    /// An org's single sign-on (track P M5).
    OrgSso {
        #[command(subcommand)]
        command: OrgSsoCommand,
    },
}

#[derive(Debug, Subcommand)]
enum OrgSsoCommand {
    /// Turn an org's identity provider and its enforcement off, for a broken provider.
    /// A running server applies it within a minute.
    Disable {
        /// Org id or slug.
        org: String,
        /// Config file. Overrides `CETRA_CONFIG`.
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum EntitlementsCommand {
    /// Print an org's resolved entitlements.
    Show {
        /// Org id or slug.
        org: String,
        /// `table` or `json`.
        #[arg(long, value_enum, default_value = "table")]
        format: output::ListFormat,
        /// Config file. Overrides `CETRA_CONFIG`.
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

fn main() {
    let code = match tokio::runtime::Builder::new_multi_thread()
        // A2 asks for at least 8 MiB per worker: DataFusion plans recursively (A2 plan § Boundaries).
        .thread_stack_size(8 * 1024 * 1024)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(async_main()),
        Err(error) => {
            let mut stderr = io::stderr().lock();
            let _ = writeln!(stderr, "starting the runtime: {error}");
            1
        }
    };
    if code != 0 {
        std::process::exit(code);
    }
}

async fn async_main() -> i32 {
    match run_cli(std::env::args_os()).await {
        Ok(()) => 0,
        Err(error) => {
            let mut stderr = io::stderr().lock();
            let _ = writeln!(stderr, "{error:#}");
            1
        }
    }
}

/// Run one CLI invocation. `main` turns `Err` into exit status 1.
async fn run_cli(
    args: impl IntoIterator<Item = impl Into<std::ffi::OsString> + Clone>,
) -> anyhow::Result<()> {
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => {
            error.print()?;
            if error.exit_code() == 0 {
                return Ok(());
            }
            std::process::exit(error.exit_code());
        }
    };
    match cli.command {
        Command::Version => print_version()?,
        Command::Health { url } => health::check(&url).await?,
        Command::Serve { config } => serve(config.as_deref()).await?,
        Command::Admin { command } => admin_command(command).await?,
        Command::Login { url, no_browser } => {
            login::login(
                &url,
                no_browser,
                &credentials::credentials_path()?,
                io::stdout(),
            )
            .await?;
        }
        Command::Logout { url } => {
            login::logout(url.as_deref(), &credentials::credentials_path()?).await?;
        }
        Command::Query(args) => {
            let stdout = io::stdout();
            let stdout_is_terminal = stdout.is_terminal();
            let (mut out, mut err, mut input) = (stdout.lock(), io::stderr().lock(), io::stdin());
            let mut streams = output::Io {
                out: &mut out,
                err: &mut err,
                input: &mut input,
                stdout_is_terminal,
            };
            query::run(
                &args,
                credentials::credentials_path,
                &mut streams,
                chrono::Utc::now(),
            )
            .await?;
        }
        Command::Traces { command } => {
            let stdout = io::stdout();
            let stdout_is_terminal = stdout.is_terminal();
            let (mut out, mut err, mut input) = (stdout.lock(), io::stderr().lock(), io::stdin());
            let mut streams = output::Io {
                out: &mut out,
                err: &mut err,
                input: &mut input,
                stdout_is_terminal,
            };
            traces::run(&command, credentials::credentials_path, &mut streams).await?;
        }
        Command::Projects { command } => {
            let stdout = io::stdout();
            let stdout_is_terminal = stdout.is_terminal();
            let (mut out, mut err, mut input) = (stdout.lock(), io::stderr().lock(), io::stdin());
            let mut streams = output::Io {
                out: &mut out,
                err: &mut err,
                input: &mut input,
                stdout_is_terminal,
            };
            projects::run(&command, credentials::credentials_path, &mut streams).await?;
        }
    }
    Ok(())
}

async fn serve(config_path: Option<&Path>) -> anyhow::Result<()> {
    let config = ServerConfig::load(config_path)
        .map_err(ServeError::from)
        .context("loading config")?;
    init_tracing(&config.log)?;
    let signals = Signals::install().context("installing signal handlers")?;
    serve::run(config, Some(cetra_ui::router()), signals.wait())
        .await
        .map_err(annotate)?;
    Ok(())
}

fn annotate(error: ServeError) -> anyhow::Error {
    let context = match &error {
        ServeError::Config(_) => "loading config",
        ServeError::Storage(_) => "opening storage",
        ServeError::Meta(_) => "opening meta.db",
        ServeError::Crypto(_) => "loading the encryption key",
        ServeError::Bind { .. } => "binding",
        ServeError::Prices(_) => "building the price table",
        ServeError::Serve(_) => "serving",
        ServeError::Init { .. } => "applying init",
    };
    anyhow::Error::from(error).context(context)
}

/// The `rmcp` crate logs whole MCP requests and results below `warn`; they can hold prompts
/// and SQL text, so its level is capped whatever `log.level` says (README § Privacy).
const RMCP_DIRECTIVE: &str = "rmcp=warn";

/// `log.level` as an `EnvFilter`, with [`RMCP_DIRECTIVE`] added last so it replaces any
/// `rmcp=…` directive in `level`.
fn log_filter(level: &str) -> anyhow::Result<EnvFilter> {
    let directive = RMCP_DIRECTIVE
        .parse()
        .context("parsing the rmcp log directive")?;
    Ok(EnvFilter::try_new(level)
        .context("loading config")?
        .add_directive(directive))
}

fn init_tracing(log: &LogConfig) -> anyhow::Result<()> {
    let filter = log_filter(&log.level)?;
    let format = log.format.unwrap_or_else(default_log_format);
    match format {
        LogFormat::Pretty => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .pretty()
                .with_writer(io::stdout)
                .init();
        }
        LogFormat::Json => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .json()
                .with_writer(io::stdout)
                .init();
        }
    }
    Ok(())
}

fn default_log_format() -> LogFormat {
    if io::stdout().is_terminal() {
        LogFormat::Pretty
    } else {
        LogFormat::Json
    }
}

fn print_version() -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "cetra {}", env!("CARGO_PKG_VERSION"))?;
    let ui = if cetra_ui::embedded() {
        "embedded"
    } else {
        "not built"
    };
    writeln!(stdout, "ui: {ui}")
}

async fn admin_command(command: AdminCommand) -> anyhow::Result<()> {
    match command {
        AdminCommand::CreateUser {
            email,
            name,
            password_stdin,
            instance_admin,
            config,
        } => {
            let password = if password_stdin {
                Some(read_password_line(io::stdin().lock())?)
            } else {
                None
            };
            let loaded = load_config(config.as_deref())?;
            let created = admin::create_user(
                &loaded,
                &email,
                name.as_deref(),
                password.as_deref(),
                instance_admin,
            )
            .await
            .context("creating the user")?;
            let mut stdout = io::stdout().lock();
            if let Some(url) = created.reset_url {
                writeln!(stdout, "Set the password at: {url}")?;
            } else {
                writeln!(stdout, "Created {}", created.id)?;
            }
        }
        AdminCommand::ResetPassword { email, config } => {
            let loaded = load_config(config.as_deref())?;
            let url = admin::reset_password(&loaded, &email)
                .await
                .context("resetting the password")?;
            writeln!(io::stdout().lock(), "{url}")?;
        }
        AdminCommand::ListUsers { format, config } => {
            let loaded = load_config(config.as_deref())?;
            let users = admin::list_users(&loaded).await.context("listing users")?;
            let mut stdout = io::stdout().lock();
            match format {
                output::ListFormat::Table => write_table(&mut stdout, &users)?,
                output::ListFormat::Json => {
                    let listed = users.iter().map(listed_user).collect::<Vec<_>>();
                    writeln!(stdout, "{}", serde_json::to_string_pretty(&listed)?)?;
                }
            }
        }
        AdminCommand::Entitlements {
            command:
                EntitlementsCommand::Show {
                    org,
                    format,
                    config,
                },
        } => {
            let loaded = load_config(config.as_deref())?;
            let report = admin::entitlements(&loaded, &org)
                .await
                .context("reading the entitlements")?;
            let mut stdout = io::stdout().lock();
            match format {
                output::ListFormat::Table => write_entitlements(&mut stdout, &report)?,
                output::ListFormat::Json => {
                    writeln!(stdout, "{}", serde_json::to_string_pretty(&report)?)?;
                }
            }
        }
        AdminCommand::OrgSso {
            command: OrgSsoCommand::Disable { org, config },
        } => {
            let loaded = load_config(config.as_deref())?;
            let changed = admin::org_sso_disable(&loaded, &org)
                .await
                .context("disabling the org's SSO")?;
            let mut stdout = io::stdout().lock();
            if changed {
                writeln!(
                    stdout,
                    "org SSO disabled for {org}; a running server applies it within a minute"
                )?;
            } else {
                writeln!(stdout, "{org} has no identity provider")?;
            }
        }
        AdminCommand::RotateEncryptionKey {
            new_from_stdin,
            config,
        } => {
            let new_key = if new_from_stdin {
                Some(read_key_line(io::stdin().lock())?)
            } else {
                None
            };
            let loaded = load_config(config.as_deref())?;
            let rotated = admin::rotate_encryption_key(&loaded, new_key.as_deref())
                .await
                .context("rotating the encryption key")?;
            let mut stdout = io::stdout().lock();
            writeln!(stdout, "Re-encrypted {} values.", rotated.values)?;
            if loaded.encryption_key.is_some() {
                writeln!(
                    stdout,
                    "Set CETRA_ENCRYPTION_KEY to the new key before the next start."
                )?;
            }
        }
        AdminCommand::Backup {
            to,
            meta_only,
            config,
        } => backup_command(&to, meta_only, config.as_deref()).await?,
        AdminCommand::GcOrphans {
            min_age,
            dry_run,
            rollback_bucket,
            config,
        } => {
            let loaded = load_config(config.as_deref())?;
            let report = admin::gc_orphans(&loaded, min_age, dry_run, rollback_bucket).await?;
            let mut stdout = io::stdout().lock();
            let verb = if report.dry_run {
                "orphans found"
            } else {
                "orphans deleted"
            };
            writeln!(
                stdout,
                "{verb}: {} ({})",
                report.orphans,
                format_bytes(report.orphan_bytes)
            )?;
            writeln!(
                stdout,
                "recent unknown objects kept: {}",
                report.recent_unknown
            )?;
            writeln!(stdout, "missing objects: {}", report.missing)?;
            if report.missing_media > 0 {
                writeln!(
                    stdout,
                    "missing media: {} (attachments only; traces are intact)",
                    report.missing_media
                )?;
            }
            if report.missing > 0 {
                anyhow::bail!(
                    "the catalog needs objects the store does not have: the restore is incomplete (backup older than gc.grace, or objects/ not copied)"
                );
            }
        }
        AdminCommand::Migrate { config } => {
            let loaded = load_config(config.as_deref())?;
            admin::migrate(&loaded)
                .await
                .context("migrating the databases")?;
            writeln!(
                io::stdout().lock(),
                "meta.db and catalog.db are at this version's schema"
            )?;
        }
        AdminCommand::ReapplyDeletions { from, config } => {
            let loaded = load_config(config.as_deref())?;
            let report = admin::reapply_deletions(&loaded, &from)
                .await
                .context("re-applying deletions")?;
            writeln!(
                io::stdout().lock(),
                "Re-opened {} deletions ({} failed ones copied for the record, {} already present). `cetra serve` applies them.",
                report.reopened,
                report.failed_copied,
                report.present
            )?;
        }
    }
    Ok(())
}

fn load_config(path: Option<&Path>) -> anyhow::Result<ServerConfig> {
    ServerConfig::load(path).context("loading config")
}

/// Asks the running server, through `<data_dir>/admin.sock`, to write a backup into `to`
/// (N M2, D-N-17). The server writes `to`, so the path is resolved here, against this
/// process's working directory.
async fn backup_command(to: &Path, meta_only: bool, config: Option<&Path>) -> anyhow::Result<()> {
    let loaded = load_config(config)?;
    let to = std::path::absolute(to).context("resolving the backup directory")?;
    let socket = loaded.data_dir.join(ADMIN_SOCKET);
    let mut stream = match UnixStream::connect(&socket).await {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            anyhow::bail!(
                "no running cetra serve answers on {}; start it, or for a stopped server copy the data directory (docs/self-hosting/backup.md)",
                socket.display()
            );
        }
        Err(error) => return Err(error).context("connecting to the admin socket"),
    };
    let request = serde_json::to_string(&AdminRequest::Backup { to, meta_only })?;
    stream
        .write_all(format!("{request}\n").as_bytes())
        .await
        .context("sending the backup request")?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .await
        .context("reading the server's answer")?;
    if line.is_empty() {
        anyhow::bail!("the server closed the connection without an answer");
    }
    match serde_json::from_str::<AdminResponse>(&line).context("reading the server's answer")? {
        AdminResponse::Done { report, .. } => {
            let mut stdout = io::stdout().lock();
            writeln!(stdout, "backup written to {}", report.dir.display())?;
            match report.mode {
                BackupMode::Full => {
                    writeln!(stdout, "mode: full")?;
                    writeln!(
                        stdout,
                        "objects: {} ({}; {} linked, {} copied)",
                        report.objects,
                        format_bytes(report.object_bytes),
                        report.linked,
                        report.copied
                    )?;
                }
                BackupMode::MetaOnly => {
                    writeln!(stdout, "mode: meta-only")?;
                    writeln!(
                        stdout,
                        "objects: {} ({}; in the bucket)",
                        report.objects,
                        format_bytes(report.object_bytes)
                    )?;
                }
            }
            writeln!(
                stdout,
                "took: {}.{} s",
                report.duration_ms / 1000,
                report.duration_ms % 1000 / 100
            )?;
            Ok(())
        }
        AdminResponse::Failed { code, message, .. } => {
            anyhow::bail!("backup failed ({code}): {message}")
        }
    }
}

/// `<n>s`, `<n>m`, `<n>h` or `<n>d`, for `--min-age`. A small parser: the workspace has no
/// `humantime` crate, and one flag does not justify adding it.
fn humantime_duration(text: &str) -> Result<Duration, String> {
    let invalid = || format!("invalid duration {text:?}: use <n>s, <n>m, <n>h or <n>d");
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (digits, unit) = text.split_at(split);
    let unit_seconds: u64 = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        _ => return Err(invalid()),
    };
    let amount: u64 = digits.parse().map_err(|_| invalid())?;
    amount
        .checked_mul(unit_seconds)
        .map(Duration::from_secs)
        .ok_or_else(|| format!("duration {text:?} is too long"))
}

/// Binary units with one decimal, for example `3.0 GiB`. Integer arithmetic only.
fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut whole = bytes;
    let mut tenth = 0;
    let mut unit = 0;
    while whole >= 1024 && unit + 1 < UNITS.len() {
        tenth = (whole % 1024) * 10 / 1024;
        whole /= 1024;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{whole}.{tenth} {}", UNITS[unit])
    }
}

/// One stdin line, with a trailing `\n` or `\r\n` removed. The password itself is not trimmed.
fn read_password_line(mut input: impl BufRead) -> anyhow::Result<String> {
    let mut line = String::new();
    let read = input.read_line(&mut line).context("reading the password")?;
    if read == 0 {
        anyhow::bail!("expected a password on stdin");
    }
    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
    Ok(line)
}

/// One stdin line, with a trailing `\n` or `\r\n` removed. Leading and trailing
/// spaces are trimmed where the key is decoded.
fn read_key_line(mut input: impl BufRead) -> anyhow::Result<String> {
    let mut line = String::new();
    let read = input.read_line(&mut line).context("reading the key")?;
    if read == 0 {
        anyhow::bail!("expected a key on stdin");
    }
    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
    Ok(line)
}

fn write_table(out: &mut impl Write, users: &[UserSummary]) -> io::Result<()> {
    writeln!(out, "id\temail\tname\tadmin\tdisabled\tcreated")?;
    for user in users {
        writeln!(
            out,
            "{}\t{}\t{}\t{}\t{}\t{}",
            user.id,
            user.email,
            user.name.as_deref().unwrap_or(""),
            yes_no(user.instance_admin),
            yes_no(user.disabled),
            output::format_time(user.created_at),
        )?;
    }
    Ok(())
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

/// `key: value` lines. Enum values print as their JSON names, instants as RFC 3339, and
/// an absent value as `-` (or `unlimited` for a limit and the retention window).
fn write_entitlements(out: &mut impl Write, report: &EntitlementsReport) -> io::Result<()> {
    let plan = &report.plan;
    let none = || "-".to_owned();
    writeln!(out, "org: {} ({})", report.org_slug, report.org_id)?;
    match &plan.id {
        Some(id) => writeln!(out, "plan: {} ({id})", plan.name)?,
        None => writeln!(out, "plan: {}", plan.name)?,
    }
    writeln!(out, "source: {}", wire_name(&plan.source))?;
    writeln!(
        out,
        "status: {}",
        plan.status.as_ref().map_or_else(none, wire_name)
    )?;
    writeln!(
        out,
        "expires: {}",
        plan.expires_at.map_or_else(none, output::format_time)
    )?;
    writeln!(
        out,
        "grace ends: {}",
        plan.grace_ends_at.map_or_else(none, output::format_time)
    )?;
    let add_ons = if plan.add_ons.is_empty() {
        "-".to_owned()
    } else {
        plan.add_ons.join(", ")
    };
    writeln!(out, "add-ons: {add_ons}")?;
    let features = report.features.iter().map(wire_name).collect::<Vec<_>>();
    writeln!(out, "features: {}", features.join(", "))?;
    for (name, limit) in limit_rows(&report.limits) {
        writeln!(out, "{name}: {}", limit_text(limit))?;
    }
    writeln!(out, "units this month: {}", report.month_units)?;
    let window = report
        .retention_window_days
        .map_or_else(|| "unlimited".to_owned(), |days| format!("{days} days"));
    writeln!(out, "retention window: {window}")
}

/// The nine limits in D-P-13 order.
fn limit_rows(limits: &OrgLimits) -> [(&'static str, Option<u64>); 9] {
    [
        ("units_included", limits.units_included),
        ("units_cap", limits.units_cap),
        ("ingest_spans_per_sec", limits.ingest_spans_per_sec),
        ("ingest_burst", limits.ingest_burst),
        ("data_access_days", limits.data_access_days),
        ("max_seats", limits.max_seats),
        ("max_projects", limits.max_projects),
        ("max_alert_rules", limits.max_alert_rules),
        ("max_active_online_rules", limits.max_active_online_rules),
    ]
}

fn limit_text(limit: Option<u64>) -> String {
    limit.map_or_else(|| "unlimited".to_owned(), |value| value.to_string())
}

/// The serde name of a unit enum value, e.g. `token` for `PlanSource::Token`.
fn wire_name(value: &impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

#[derive(serde::Serialize)]
struct ListedUser {
    id: String,
    email: String,
    name: Option<String>,
    instance_admin: bool,
    disabled: bool,
    created_at: String,
}

fn listed_user(user: &UserSummary) -> ListedUser {
    ListedUser {
        id: user.id.to_string(),
        email: user.email.clone(),
        name: user.name.clone(),
        instance_admin: user.instance_admin,
        disabled: user.disabled,
        created_at: output::format_time(user.created_at),
    }
}

/// SIGINT and SIGTERM. A second signal exits with status 130.
struct Signals {
    #[cfg(unix)]
    term: tokio::signal::unix::Signal,
    #[cfg(unix)]
    int: tokio::signal::unix::Signal,
}

impl Signals {
    fn install() -> anyhow::Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Self {
                term: signal(SignalKind::terminate()).context("installing SIGTERM handler")?,
                int: signal(SignalKind::interrupt()).context("installing SIGINT handler")?,
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {})
        }
    }

    async fn wait(mut self) {
        self.recv().await;
        tokio::spawn(async move {
            self.recv().await;
            std::process::exit(130);
        });
    }

    async fn recv(&mut self) {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = self.term.recv() => {}
                _ = self.int.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use clap::Parser;

    use super::{
        AdminCommand, Cli, Command, EntitlementsCommand, OrgSsoCommand, humantime_duration,
        log_filter, run_cli,
    };
    use crate::output;
    use crate::test_server::TestServer;
    use cetra_server::admin::EntitlementsReport;
    use cetra_server::api::entitlements::{Feature, OrgLimits};
    use std::time::Duration;

    #[test]
    fn log_filter_caps_rmcp() {
        for level in ["info", "trace", "info,rmcp=trace"] {
            let filter = log_filter(level).unwrap().to_string();
            assert!(filter.contains("rmcp=warn"), "{level}: {filter}");
            assert!(!filter.contains("rmcp=trace"), "{level}: {filter}");
        }
    }

    /// `log_filter("debug")` admits debug events, but not rmcp's: a subscriber built from it
    /// drops an rmcp event (rmcp logs whole requests and results at debug) and still emits a
    /// debug event for another target.
    #[test]
    fn a_debug_filter_drops_rmcp_debug_events() {
        use tracing_subscriber::Layer;
        use tracing_subscriber::layer::SubscriberExt;

        let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = Arc::clone(&buffer);
        let layer = tracing_subscriber::fmt::layer()
            .with_writer(move || Buffer(Arc::clone(&writer)))
            .with_ansi(false)
            .with_target(true)
            .with_filter(log_filter("debug").unwrap());
        let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(layer));
        let guard = tracing::dispatcher::set_default(&dispatch);
        tracing::debug!(target: "rmcp::service", "dropped-canary");
        tracing::debug!(target: "cetra::serve", "kept-canary");
        drop(guard);
        drop(dispatch);
        let logs = String::from_utf8_lossy(&buffer.lock().unwrap().clone()).into_owned();
        assert!(!logs.contains("dropped-canary"), "{logs}");
        assert!(logs.contains("kept-canary"), "{logs}");
        assert!(logs.contains("cetra::serve"), "{logs}");
    }

    /// `io::Write` over the shared buffer of [`a_debug_filter_drops_rmcp_debug_events`].
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Buffer {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(data)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn health_parses_with_and_without_url() {
        let url_of = |argv: &[&str]| match Cli::try_parse_from(argv).unwrap().command {
            Command::Health { url } => url,
            other => panic!("unexpected command {other:?}"),
        };
        assert_eq!(url_of(&["cetra", "health"]), "http://127.0.0.1:8080");
        assert_eq!(
            url_of(&["cetra", "health", "--url", "http://10.0.0.1:9000"]),
            "http://10.0.0.1:9000"
        );
    }

    #[test]
    fn rotate_encryption_key_parses() {
        let cli = Cli::try_parse_from([
            "cetra",
            "admin",
            "rotate-encryption-key",
            "--new-from-stdin",
        ])
        .unwrap();
        let Command::Admin {
            command: AdminCommand::RotateEncryptionKey { new_from_stdin, .. },
        } = cli.command
        else {
            panic!("expected the rotate-encryption-key command");
        };
        assert!(new_from_stdin);
        assert!(
            Cli::try_parse_from(["cetra", "admin", "rotate-encryption-key", "--new", "x"]).is_err()
        );
    }

    #[tokio::test]
    async fn missing_config_exits_with_the_path() {
        let path = std::env::temp_dir().join(format!("cetra-missing-{}.toml", std::process::id()));
        let error = run_cli(["cetra", "serve", "--config", path.to_str().unwrap()])
            .await
            .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("loading config"), "{text}");
        assert!(text.contains(&path.display().to_string()), "{text}");
    }

    #[test]
    fn admin_list_users_keeps_its_format_flag() {
        let format_of = |argv: &[&str]| match Cli::try_parse_from(argv).unwrap().command {
            Command::Admin {
                command: AdminCommand::ListUsers { format, .. },
            } => format,
            other => panic!("unexpected command {other:?}"),
        };
        assert_eq!(
            format_of(&["cetra", "admin", "list-users", "--format", "json"]),
            output::ListFormat::Json
        );
        assert_eq!(
            format_of(&["cetra", "admin", "list-users"]),
            output::ListFormat::Table
        );
        let error =
            Cli::try_parse_from(["cetra", "admin", "list-users", "--format", "xml"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::InvalidValue);
    }

    #[test]
    fn admin_org_sso_disable_takes_an_org() {
        let parsed = Cli::try_parse_from(["cetra", "admin", "org-sso", "disable", "acme"]).unwrap();
        match parsed.command {
            Command::Admin {
                command:
                    AdminCommand::OrgSso {
                        command: OrgSsoCommand::Disable { org, config },
                    },
            } => {
                assert_eq!(org, "acme");
                assert_eq!(config, None);
            }
            other => panic!("unexpected command {other:?}"),
        }
        assert!(Cli::try_parse_from(["cetra", "admin", "org-sso", "disable"]).is_err());
    }

    #[test]
    fn admin_entitlements_show_parses_its_flags() {
        type Show = (String, output::ListFormat, Option<PathBuf>);
        let show_of = |argv: &[&str]| -> Show {
            match Cli::try_parse_from(argv).unwrap().command {
                Command::Admin {
                    command:
                        AdminCommand::Entitlements {
                            command:
                                EntitlementsCommand::Show {
                                    org,
                                    format,
                                    config,
                                },
                        },
                } => (org, format, config),
                other => panic!("unexpected command {other:?}"),
            }
        };
        assert_eq!(
            show_of(&["cetra", "admin", "entitlements", "show", "acme"]),
            ("acme".to_owned(), output::ListFormat::Table, None)
        );
        assert_eq!(
            show_of(&[
                "cetra",
                "admin",
                "entitlements",
                "show",
                "acme",
                "--format",
                "json",
                "--config",
                "/etc/cetra.toml",
            ]),
            (
                "acme".to_owned(),
                output::ListFormat::Json,
                Some(PathBuf::from("/etc/cetra.toml"))
            )
        );
        assert!(Cli::try_parse_from(["cetra", "admin", "entitlements", "show"]).is_err());
    }

    #[test]
    fn entitlements_table_names_the_plan_and_limits() {
        use cetra_core::ids::OrgId;
        use cetra_server::api::entitlements::{OrgPlan, PlanSource, PlanStatus};

        let report = EntitlementsReport {
            org_id: OrgId::new_v7().as_uuid(),
            org_slug: "acme".to_owned(),
            plan: OrgPlan {
                id: Some("pro".to_owned()),
                name: "Pro".to_owned(),
                add_ons: vec!["teams".to_owned()],
                source: PlanSource::Token,
                status: Some(PlanStatus::Grace),
                expires_at: Some(1_767_225_600_000_000_000),
                grace_ends_at: None,
            },
            features: vec![Feature::RetentionManagement, Feature::ProjectRoles],
            limits: OrgLimits {
                units_included: Some(200_000),
                ..OrgLimits::default()
            },
            retention_window_days: None,
            month_units: 1240,
        };
        let mut out = Vec::new();
        super::write_entitlements(&mut out, &report).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("plan: Pro (pro)\n"), "{text}");
        assert!(text.contains("source: token\n"), "{text}");
        assert!(text.contains("status: grace\n"), "{text}");
        assert!(text.contains("grace ends: -\n"), "{text}");
        assert!(text.contains("add-ons: teams\n"), "{text}");
        assert!(
            text.contains("features: retention_management, project_roles\n"),
            "{text}"
        );
        assert!(text.contains("units_included: 200000\n"), "{text}");
        assert!(text.contains("units_cap: unlimited\n"), "{text}");
        assert!(text.contains("retention window: unlimited\n"), "{text}");
        assert!(text.contains("units this month: 1240\n"), "{text}");
    }

    #[test]
    fn password_line_drops_one_trailing_newline() {
        let line = super::read_password_line("secret\r\n".as_bytes()).unwrap();
        assert_eq!(line, "secret");
        let spaced = super::read_password_line(" secret \n".as_bytes()).unwrap();
        assert_eq!(spaced, " secret ");
        let eof = super::read_password_line("secret".as_bytes()).unwrap();
        assert_eq!(eof, "secret");
        assert!(super::read_password_line("".as_bytes()).is_err());
    }

    #[test]
    fn key_line_drops_one_trailing_newline() {
        let crlf = super::read_key_line("key-line\r\n".as_bytes()).unwrap();
        assert_eq!(crlf, "key-line");
        let eof = super::read_key_line("key-line".as_bytes()).unwrap();
        assert_eq!(eof, "key-line");
        let error = super::read_key_line("".as_bytes()).unwrap_err();
        assert_eq!(error.to_string(), "expected a key on stdin");
    }

    #[tokio::test]
    async fn list_users_on_a_missing_database_names_serve() {
        let dir = std::env::temp_dir().join(format!(
            "cetra-admin-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("cetra.toml");
        let data = dir.join("data");
        std::fs::write(&config_path, format!("data_dir = \"{}\"\n", data.display())).unwrap();
        let error = run_cli([
            "cetra",
            "admin",
            "list-users",
            "--config",
            config_path.to_str().unwrap(),
        ])
        .await
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(
            text.contains("run `cetra serve` once to create the database"),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reapply_deletions_requires_from() {
        assert!(Cli::try_parse_from(["cetra", "admin", "reapply-deletions"]).is_err());
        let parsed =
            Cli::try_parse_from(["cetra", "admin", "reapply-deletions", "--from", "/x"]).unwrap();
        assert!(matches!(
            parsed.command,
            Command::Admin {
                command: AdminCommand::ReapplyDeletions { .. }
            }
        ));
    }

    #[tokio::test]
    async fn reapply_deletions_on_a_missing_database_names_serve() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("cetra-admin-{}-{stamp}", std::process::id()));
        let from =
            std::env::temp_dir().join(format!("cetra-admin-from-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&from).unwrap();
        let config_path = dir.join("cetra.toml");
        let data = dir.join("data");
        std::fs::write(&config_path, format!("data_dir = \"{}\"\n", data.display())).unwrap();
        let error = run_cli([
            "cetra",
            "admin",
            "reapply-deletions",
            "--from",
            from.to_str().unwrap(),
            "--config",
            config_path.to_str().unwrap(),
        ])
        .await
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(
            text.contains("run `cetra serve` once to create the database"),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&from);
    }

    #[test]
    fn backup_parses() {
        let parse = |argv: &[&str]| match Cli::try_parse_from(argv).unwrap().command {
            Command::Admin {
                command: AdminCommand::Backup { to, meta_only, .. },
            } => (to, meta_only),
            _ => panic!("expected the backup command"),
        };
        let (to, meta_only) = parse(&["cetra", "admin", "backup", "--to", "/b"]);
        assert_eq!(to, PathBuf::from("/b"));
        assert!(!meta_only);
        let (_, meta_only) = parse(&["cetra", "admin", "backup", "--to", "/b", "--meta-only"]);
        assert!(meta_only);
    }

    #[test]
    fn gc_orphans_parses_defaults() {
        let parse = |argv: &[&str]| match Cli::try_parse_from(argv).unwrap().command {
            Command::Admin {
                command:
                    AdminCommand::GcOrphans {
                        min_age, dry_run, ..
                    },
            } => (min_age, dry_run),
            _ => panic!("expected the gc-orphans command"),
        };
        assert_eq!(
            parse(&["cetra", "admin", "gc-orphans"]),
            (Duration::from_secs(3_600), false)
        );
        assert_eq!(
            parse(&["cetra", "admin", "gc-orphans", "--min-age", "30m"]),
            (Duration::from_secs(1_800), false)
        );
        assert_eq!(
            parse(&["cetra", "admin", "gc-orphans", "--dry-run"]),
            (Duration::from_secs(3_600), true)
        );
    }

    #[test]
    fn duration_flag_parses_units() {
        assert_eq!(humantime_duration("30s"), Ok(Duration::from_secs(30)));
        assert_eq!(humantime_duration("30m"), Ok(Duration::from_secs(1_800)));
        assert_eq!(humantime_duration("2h"), Ok(Duration::from_secs(7_200)));
        assert_eq!(humantime_duration("1d"), Ok(Duration::from_secs(86_400)));
        for bad in ["", "m", "10", "10w", "1.5h", "-1m"] {
            assert!(humantime_duration(bad).is_err(), "{bad}");
        }
    }

    /// A config file that sets only the data directory and disables the OTLP listeners.
    fn config_file(dir: &Path, data: &Path) -> PathBuf {
        let path = dir.join("cetra.toml");
        std::fs::write(
            &path,
            format!(
                "data_dir = {:?}\notlp_http_addr = \"\"\ngrpc_addr = \"\"\n",
                data.display().to_string()
            ),
        )
        .unwrap();
        path
    }

    /// The first `.parquet` file under `dir`, searched depth first.
    fn first_parquet_under(dir: &Path) -> Option<PathBuf> {
        for entry in std::fs::read_dir(dir).ok()? {
            let path = entry.ok()?.path();
            let found = if path.is_dir() {
                first_parquet_under(&path)
            } else {
                Some(path).filter(|file| file.extension().is_some_and(|ext| ext == "parquet"))
            };
            if found.is_some() {
                return found;
            }
        }
        None
    }

    #[tokio::test]
    async fn backup_without_a_server_names_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_file(dir.path(), &dir.path().join("data"));
        let target = dir.path().join("backup");
        let error = run_cli([
            "cetra",
            "admin",
            "backup",
            "--to",
            target.to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
        ])
        .await
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("admin.sock"), "{text}");
        assert!(text.contains("docs/self-hosting/backup.md"), "{text}");
    }

    #[tokio::test]
    async fn backup_through_a_running_server() {
        let server = TestServer::start().await;
        let target_root = tempfile::tempdir().unwrap();
        let target = target_root.path().join("backup");
        run_cli([
            "cetra",
            "admin",
            "backup",
            "--to",
            target.to_str().unwrap(),
            "--config",
            server.config_path().to_str().unwrap(),
        ])
        .await
        .unwrap();
        assert!(target.join("manifest.json").is_file());
        server.stop().await;
    }

    #[tokio::test]
    async fn backup_meta_only_in_local_mode_is_refused() {
        let server = TestServer::start().await;
        let target_root = tempfile::tempdir().unwrap();
        let target = target_root.path().join("backup");
        let error = run_cli([
            "cetra",
            "admin",
            "backup",
            "--to",
            target.to_str().unwrap(),
            "--meta-only",
            "--config",
            server.config_path().to_str().unwrap(),
        ])
        .await
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("meta_only_needs_s3"), "{text}");
        assert!(!target.exists());
        server.stop().await;
    }

    #[tokio::test]
    async fn gc_orphans_refuses_a_data_dir_without_a_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_file(dir.path(), &dir.path().join("data"));
        let error = run_cli([
            "cetra",
            "admin",
            "gc-orphans",
            "--config",
            config.to_str().unwrap(),
        ])
        .await
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("no catalog.db"), "{text}");
    }

    #[tokio::test]
    async fn gc_orphans_on_a_stopped_server_reports_zero_missing() {
        let server = TestServer::start().await;
        let config = server.config_path();
        let _dir = server.stop().await;
        run_cli([
            "cetra",
            "admin",
            "gc-orphans",
            "--config",
            config.to_str().unwrap(),
        ])
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn gc_orphans_fails_when_objects_are_missing() {
        let server = TestServer::start().await;
        let data = server.data_dir();
        let config = server.config_path();
        let _dir = server.stop().await;
        // The stop flushed the trace to Parquet; removing one file makes the catalog need
        // an object the store does not have.
        let parquet = first_parquet_under(&data.join("objects").join("tables"))
            .expect("a flushed Parquet file");
        std::fs::remove_file(&parquet).unwrap();
        let error = run_cli([
            "cetra",
            "admin",
            "gc-orphans",
            "--config",
            config.to_str().unwrap(),
        ])
        .await
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("the restore is incomplete"), "{text}");
    }

    #[tokio::test]
    async fn migrate_on_a_stopped_server_succeeds() {
        let server = TestServer::start().await;
        let config = server.config_path();
        let _dir = server.stop().await;
        run_cli([
            "cetra",
            "admin",
            "migrate",
            "--config",
            config.to_str().unwrap(),
        ])
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn migrate_while_the_server_runs_is_refused() {
        let server = TestServer::start().await;
        let meta = server.data_dir().join("meta.db");
        let meta_before = std::fs::read(&meta).unwrap();
        let error = run_cli([
            "cetra",
            "admin",
            "migrate",
            "--config",
            server.config_path().to_str().unwrap(),
        ])
        .await
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(
            text.contains("the server is running: stop it first"),
            "{text}"
        );
        // The lock is taken before meta.db is opened, so a refused migrate writes nothing.
        assert_eq!(std::fs::read(&meta).unwrap(), meta_before);
        server.stop().await;
    }

    #[tokio::test]
    async fn migrate_without_a_data_dir_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let config = config_file(dir.path(), &data);
        let error = run_cli([
            "cetra",
            "admin",
            "migrate",
            "--config",
            config.to_str().unwrap(),
        ])
        .await
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("no catalog.db"), "{text}");
        assert!(!data.join("meta.db").exists());
        assert!(!data.join("catalog.db").exists());
    }
}
