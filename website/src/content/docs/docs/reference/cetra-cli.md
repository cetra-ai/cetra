---
title: cetra-cli
description: "Every cetra-cli command and option: init, query and login."
---

`cetra-cli` is the setup CLI of the `@cetra-ai/cli` package. Run it with `npx @cetra-ai/cli <command>`, or install it and run `cetra-cli <command>`. It needs Node.js 22.12 or later. It has three commands: `init` sets an app up, `query` runs one SQL statement on the project, and `login` saves a new token.

Human text goes to stderr. With `--json`, the result goes to stdout, and it never contains a key or a token. Run `cetra-cli <command> --help` for the options.

### `cetra-cli init`

Signs in, picks a project, writes the env file, the project link and the skill, and prints the prompt for your coding agent. With no server at `--url` on this machine, it starts a local one first. See [Set up with your coding agent](/docs/get-started/coding-agent/) for the steps.

```bash
npx @cetra-ai/cli@latest init [--url <URL>] [--project <org/project>] [--dir <PATH>]
```

| Option | What it does |
|---|---|
| `--url <URL>` | The server. Defaults to `CETRA_URL`, then the project link, then the cloud (`https://app.cetra.sh`). Use `http://localhost:<port>` for a self-hosted server on this machine. |
| `--project <org/project>` | Preselects the project on the approval page. Ignored when `init` starts a local server, which has one project. |
| `--dir <PATH>` | The project directory. Defaults to the current directory. It must exist. |
| `--env-file <PATH>` | The env file, relative to `--dir`. Defaults to `.env`. |
| `--agents <LIST>` | Where the skill goes: `claude`, `agents`, or both as a comma list. Defaults to both. An unknown name is a usage error. |
| `--no-skill` | Does not install the skill. Cannot be combined with `--agents`. |
| `--no-browser` | Prints the sign-in URL and does not open a browser. |
| `--yes` | Starts a local server without asking, uses the default admin email `admin@cetra.local` on its first start, and replaces an existing `CETRA_API_KEY` for the same server without asking. |
| `--json` | Prints one JSON object on stdout. |
| `-h`, `--help` | Shows the options. |

With `--json`, the object has these fields: `server`, `project` (`org/project`), `env_file`, `link_file`, `skills` (the installed directories), `prompt` and `traces_url`.

If `.env` already has a `CETRA_API_KEY` for the same server, `init` asks before it replaces the key. Without a terminal and without `--yes`, it keeps the key and says so.

On a first start of a local server, `init` prints the admin email and the generated password, and the UI address. The password is printed only when stderr is a terminal. Otherwise it is in no file, and you reset it with `cetra admin reset-password <EMAIL>`.

### `cetra-cli query`

Runs one SQL statement on the linked project and prints the rows.

```bash
npx @cetra-ai/cli query "<SQL>" [--since 1h] [--limit 100]
```

The statement is one positional argument, in quotes. The server comes from `--url`, then `CETRA_URL`, then the project link. The project comes from `--project`, then `CETRA_PROJECT`, then the project link. A server named by `--url` or `CETRA_URL` uses `CETRA_TOKEN` when it is set. Otherwise the saved token for that server is used.

| Option | What it does |
|---|---|
| `--since <DURATION>` | The window ending now: `30m`, `24h` or `7d`. Units are `s`, `m`, `h` and `d`. Defaults to `24h`. |
| `--from <TIME>` | The window start, as RFC 3339. Needs `--to`, and cannot be combined with `--since`. |
| `--to <TIME>` | The window end, as RFC 3339, exclusive. Needs `--from`. |
| `--limit <N>` | The most rows, from 1 to 10000. Defaults to 100. |
| `--json` | Prints `{"columns": [...], "rows": [...], "truncated": false}` on stdout. Each column has a `name` and a `type`. |
| `--url <URL>` | The server, as for `init`. |
| `--project <org/project>` | The project, as above. |
| `-h`, `--help` | Shows the options. |

The table goes to stdout and the row count to stderr. When the server has more rows than `--limit`, the count says so. The `--json` output is not the Rust `cetra query` output: it is the object above, not a bare array of rows.

The check step of the coding agent prompt runs this statement on the `traces` view:

```bash
npx @cetra-ai/cli query "SELECT trace_id, name, start_time, span_count, llm_count, error_count FROM traces ORDER BY start_time DESC LIMIT 5" --since 15m --json
```

### `cetra-cli login`

Signs in again with the device flow and saves a new token. The token lasts 90 days. Run `login` again when `query` exits with code 4.

```bash
npx @cetra-ai/cli login [--url <URL>]
```

The server comes from `--url`, then `CETRA_URL`, then the only saved server. With several saved servers, pass `--url`. With none, `login` exits with a usage error (code 2) that asks for `--url`.

| Option | What it does |
|---|---|
| `--url <URL>` | The server. |
| `--no-browser` | Prints the sign-in URL and does not open a browser. |
| `--json` | Prints `{"server": "<URL>"}` on stdout. |
| `-h`, `--help` | Shows the options. |

After sign-in, `login` checks the token with the server and saves it. It does not write any project files.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | The command succeeded. |
| 1 | Any other error, such as an unreadable or invalid credentials file, or an invalid project link. |
| 2 | A usage error: an unknown option, a bad value, or no server or project to use. |
| 3 | The server could not be reached, or no local server could be started. |
| 4 | Sign-in failed: the device flow failed or expired, the server rejected the token, there is no saved token, or the server answered 401 to `query`. |
| 5 | A file could not be written: the env file, `.gitignore`, the project link or the credentials file. |
| 6 | The server rejected the SQL statement. |

## Files

### Files `init` writes

| File | What changes |
|---|---|
| The env file (`.env` by default) | `CETRA_BASE_URL` and `CETRA_API_KEY`. Each replaces its line in place, and other lines stay. The file is created with mode 0600. |
| `.gitignore` | The env file's path is appended when the directory is in a git work tree and git does not already ignore the file. |
| `.cetra/project.json` | The project link: `server`, `org`, `project` and `project_id`. It holds no secret, so you can commit it. |
| `credentials.toml` in the config directory | The personal access token for the server, as `[servers."<URL>"]` with `token = "…"`. Tokens for other servers stay. The file is created with mode 0600. |
| `.claude/skills/cetra/` and `.agents/skills/cetra/` | The skill. Each directory has a `.cetra-skill` marker. A directory that exists without the marker is left alone, and `init` prints a warning. |

The skill directories are at the top of the git work tree that holds `--dir`, or in `--dir` when it is not in a git repository.

When `init` starts a local server with `cetra serve`, it writes `local.toml` and the server's data directory, with `server.log` and `server.pid` in it. On Linux these are under `$XDG_DATA_HOME/cetra` (default `~/.local/share/cetra`). With Docker, the server runs in the `cetra-local` container, on the `cetra-local` volume. A failed first start removes what it created.

### Credentials and link locations

| Platform | `credentials.toml` |
|---|---|
| Linux | `$XDG_CONFIG_HOME/cetra/credentials.toml` when `XDG_CONFIG_HOME` is absolute, else `~/.config/cetra/credentials.toml` |
| macOS | `~/Library/Application Support/cetra/credentials.toml` |
| Windows | `%APPDATA%\cetra\credentials.toml` |

The file is shared with the `cetra` binary, so a token from `cetra login` for the same server is replaced by the one `cetra-cli` saves. The project link is always `.cetra/project.json` in the project directory, or in the nearest parent that has one.

## Environment variables

| Variable | Used by | What it does |
|---|---|---|
| `CETRA_URL` | `init`, `query`, `login` | The server, when no `--url` is given. |
| `CETRA_PROJECT` | `query` | The project, as `org/project`, when no `--project` is given. |
| `CETRA_TOKEN` | `query` | A personal access token. It is sent only to a server that `--url` or `CETRA_URL` names. A server from the project link never gets it. |
| `XDG_CONFIG_HOME` | `init`, `query`, `login` | Where `credentials.toml` is kept on Linux. |
| `XDG_DATA_HOME` | `init` | Where the local server keeps its data on Linux. |
