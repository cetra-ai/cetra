import { optionsHelp, parseOptions } from "./args.js";
import { credentialsPath, loadCredentials, serverUrl } from "./credentials.js";
import { CliError, type Deps, Exit, type ExitCode, type Io } from "./errors.js";
import { problemMessage, requestJson } from "./http.js";
import { findLink, LINK_FILE, type ProjectLink } from "./link.js";
import { sinceWindow } from "./since.js";
import { renderTable, type TableColumn } from "./table.js";

const SUMMARY = "Run one SQL statement on the linked project.";
const LINES: [string, string][] = [
  ["--since <DURATION>", "The window ending now: 30m, 24h or 7d. Default 24h"],
  ["--from <TIME>", "The window start, RFC 3339. Needs --to"],
  ["--to <TIME>", "The window end (exclusive), RFC 3339. Needs --from"],
  ["--limit <N>", "Most rows, from 1 to 10000. Default 100"],
  ["--json", "Print {columns, rows, truncated} on stdout"],
  ["--url <URL>", "The server. Defaults to CETRA_URL, then the project link"],
  ["--project <PROJECT>", "org/project. Defaults to CETRA_PROJECT, then the project link"],
  ["-h, --help", "Show this help"],
];

/** The answer of `POST /api/v1/projects/{project}/sql` (`SqlQueryResult`). */
interface SqlResult {
  columns: TableColumn[];
  rows: Record<string, unknown>[];
  truncated: boolean;
}

/** `cetra-cli query "<sql>"`: one statement on the linked project, as a table or as JSON.
 *  Human text goes to stderr; results go to stdout. */
export async function query(args: string[], io: Io, deps: Deps = {}): Promise<ExitCode> {
  const { values, positionals } = parseOptions(args, {
    since: { type: "string" },
    from: { type: "string" },
    to: { type: "string" },
    limit: { type: "string" },
    json: { type: "boolean" },
    url: { type: "string" },
    project: { type: "string" },
    help: { type: "boolean", short: "h" },
  });
  if (values.help === true) {
    io.stderr(optionsHelp("query", SUMMARY, LINES));
    return Exit.Ok;
  }
  if (positionals.length !== 1) {
    throw new CliError("query takes one SQL statement in quotes", Exit.Usage);
  }
  const statement = positionals[0] ?? "";
  if (statement.trim() === "") throw new CliError("the query is empty", Exit.Usage);

  const now = new Date(deps.now?.() ?? Date.now());
  let window: { from: string; to: string };
  if (values.from !== undefined || values.to !== undefined) {
    if (values.from === undefined || values.to === undefined) {
      throw new CliError("--from and --to need each other", Exit.Usage);
    }
    if (values.since !== undefined) {
      throw new CliError("--since cannot be combined with --from and --to", Exit.Usage);
    }
    window = { from: values.from, to: values.to };
  } else {
    window = sinceWindow(values.since ?? "24h", now);
  }
  const limit = parseLimit(values.limit);

  // The server comes from --url or CETRA_URL, else from the project link. The project comes
  // from --project or CETRA_PROJECT, else from the link. The link is read only when needed.
  const flagServer = values.url ?? nonEmpty(io.env.CETRA_URL);
  const flagProject = values.project ?? nonEmpty(io.env.CETRA_PROJECT);
  const link =
    flagServer === undefined || flagProject === undefined ? await findLink(io.cwd) : undefined;

  let server: string;
  if (flagServer !== undefined) {
    server = serverUrl(flagServer);
  } else if (link !== undefined) {
    server = linkServer(link);
  } else {
    throw new CliError("no server: run `cetra-cli init`, or pass --url", Exit.Usage);
  }
  let project: string;
  if (flagProject !== undefined) {
    project = flagProject;
  } else if (link !== undefined) {
    project = `${link.org}/${link.project}`;
  } else {
    throw new CliError("no project: run `cetra-cli init`, or pass --project", Exit.Usage);
  }

  // CETRA_TOKEN is sent only to a server the user named (--url or CETRA_URL). A server from the
  // link file never gets an environment token: the link can be committed, so it may name any
  // server. Otherwise the token is the saved one for that server.
  const fromFlag = flagServer !== undefined;
  let token = fromFlag ? nonEmpty(io.env.CETRA_TOKEN) : undefined;
  if (token === undefined) {
    token = (await loadCredentials(credentialsPath(io.env))).get(server);
  }
  if (token === undefined) {
    throw new CliError(
      `no saved token for ${server}; run \`cetra-cli login --url ${server}\``,
      Exit.SignIn,
    );
  }

  const answer = await requestJson(
    "POST",
    `${server}/api/v1/projects/${encodeURIComponent(project)}/sql`,
    {
      body: { query: statement, from: window.from, to: window.to, limit },
      token,
      timeoutMs: 90_000,
    },
  );
  if (answer.status < 200 || answer.status > 299) {
    const code = answer.status === 401 ? Exit.SignIn : Exit.Query;
    throw new CliError(problemMessage(answer.status, answer.body, server), code);
  }
  const result = sqlResult(answer.body);

  if (values.json === true) {
    io.stdout(JSON.stringify(result));
    return Exit.Ok;
  }
  const table = renderTable(result.columns, result.rows);
  if (table !== "") {
    for (const line of table.slice(0, -1).split("\n")) io.stdout(line);
  }
  const count = result.rows.length;
  const rows = count === 1 ? "1 row" : `${count} rows`;
  io.stderr(result.truncated ? `${rows}; more rows matched, raise --limit` : rows);
  return Exit.Ok;
}

/** `--limit` (or the default 100): a whole number from 1 to 10000. */
function parseLimit(text: string | undefined): number {
  if (text === undefined) return 100;
  const value = /^[0-9]+$/.test(text) ? Number(text) : Number.NaN;
  if (!(value >= 1 && value <= 10_000)) {
    throw new CliError("--limit expects a whole number from 1 to 10000", Exit.Usage);
  }
  return value;
}

/** The server a project link names, checked as `serverUrl` does; a bad one makes the link invalid. */
function linkServer(link: ProjectLink): string {
  try {
    return serverUrl(link.server);
  } catch {
    throw new CliError(`${LINK_FILE} is invalid`);
  }
}

/** The answer's `columns`, `rows` and `truncated`; anything else is an invalid response. */
function sqlResult(body: unknown): SqlResult {
  const invalid = () => new CliError("the server sent an invalid response");
  if (
    !isRecord(body) ||
    !Array.isArray(body.columns) ||
    !Array.isArray(body.rows) ||
    typeof body.truncated !== "boolean"
  ) {
    throw invalid();
  }
  const columns: TableColumn[] = [];
  for (const column of body.columns) {
    if (!isRecord(column) || typeof column.name !== "string" || typeof column.type !== "string") {
      throw invalid();
    }
    columns.push({ name: column.name, type: column.type });
  }
  const rows: Record<string, unknown>[] = [];
  for (const row of body.rows) {
    if (!isRecord(row)) throw invalid();
    rows.push(row);
  }
  return { columns, rows, truncated: body.truncated };
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** An environment value that is empty or only spaces counts as unset. */
function nonEmpty(value: string | undefined): string | undefined {
  return value === undefined || value.trim() === "" ? undefined : value;
}
