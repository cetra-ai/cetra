import { readFile, stat } from "node:fs/promises";
import { basename, isAbsolute, relative, resolve, sep } from "node:path";
import { optionsHelp, parseOptions } from "./args.js";
import { credentialsPath, saveToken, serverUrl } from "./credentials.js";
import { deviceSignIn } from "./device.js";
import { ignoreInGit, readVariable, setVariables, writeEnvFile } from "./envfile.js";
import { CliError, type Deps, Exit, type ExitCode, type Io } from "./errors.js";
import { localHost } from "./host.js";
import { findLink, LINK_FILE, type ProjectLink, writeLink } from "./link.js";
import { isLoopback, isReady, passwordSignIn, type Started, startLocal } from "./local.js";
import { agentPrompt } from "./prompt.js";
import { yes as affirmative, ask, interactive } from "./prompt-io.js";
import { bundledSkill, installSkill, SKILL_TARGETS, skillRoot } from "./skill.js";
import { VERSION } from "./version.js";

const SUMMARY = "Sign in, pick a project, write .env and install the cetra skill.";
const CLOUD = "https://app.cetra.sh";
const LINES: [string, string][] = [
  ["--url <URL>", "The server. Defaults to CETRA_URL, then the project link, then the cloud"],
  ["--project <org/project>", "Preselect the project on the approval page"],
  ["--dir <PATH>", "The project directory. Defaults to the current directory"],
  ["--env-file <PATH>", "The env file, relative to --dir. Defaults to .env"],
  ["--agents <LIST>", "Where the skill goes: claude, agents or both (default)"],
  ["--no-skill", "Do not install the skill"],
  ["--no-browser", "Print the sign-in URL and do not open a browser"],
  ["--yes", "Start a local server and replace an existing API key without asking"],
  ["--json", "Print one JSON object on stdout"],
  ["-h, --help", "Show this help"],
];
const NOTE = "--project is ignored when init starts a local server: a first start has one project.";

type Target = keyof typeof SKILL_TARGETS;

/** The signed-in result `init` writes: the server the CLI talked to, its grant and the secrets. */
interface SignedIn {
  grantServer: string;
  token: string;
  key: string;
  org: string;
  project: string;
  projectId: string;
}

/** `cetra-cli init` (design § `@cetra-ai/cli` § `init`): choose the server, start a local one if
 *  needed, sign in, save the token, write `.env`, the project link and the skill, and print the
 *  agent prompt. Human text goes to stderr; `--json` is the only thing on stdout. */
export async function init(args: string[], io: Io, deps: Deps = {}): Promise<ExitCode> {
  const { values, positionals } = parseOptions(args, {
    url: { type: "string" },
    project: { type: "string" },
    dir: { type: "string" },
    "env-file": { type: "string" },
    agents: { type: "string" },
    "no-skill": { type: "boolean" },
    "no-browser": { type: "boolean" },
    yes: { type: "boolean" },
    json: { type: "boolean" },
    help: { type: "boolean", short: "h" },
  });
  if (values.help === true) {
    io.stderr(optionsHelp("init", SUMMARY, LINES));
    io.stderr(NOTE);
    return Exit.Ok;
  }
  if (positionals.length > 0) throw new CliError("init takes no positional arguments", Exit.Usage);

  const targets = skillTargets(values.agents, values["no-skill"] === true);
  const dir = await projectDirectory(values.dir, io.cwd);
  const envPath = resolve(dir, values["env-file"] ?? ".env");
  const noBrowser = values["no-browser"] === true;
  const yesToAll = values.yes === true;

  // 1. The server.
  let server = serverUrl(await chosenServer(values.url, io.env, dir));
  checkEnvValue(server);

  // 2. A server that does not answer: start one, if the URL is on this machine.
  let started: Started | undefined;
  if (!(await isReady(server))) {
    const url = new URL(server);
    if (!isLoopback(url)) throw new CliError(`no cetra server at ${server}`, Exit.Unreachable);
    started = await startLocal(url, dir, { yes: yesToAll, io, deps });
    server = started.server;
  }

  // 3. Sign in: on a local first start with the generated password, else the device flow.
  const signedIn: SignedIn =
    started?.firstStart === true && started.email !== undefined && started.password !== undefined
      ? await signInWithPassword(server, started.email, started.password)
      : await signInWithDevice(server, values.project, noBrowser, io, deps);

  // 4. The token, under the URL the CLI talked to.
  await saveToken(credentialsPath(io.env), server, signedIn.token);

  // 5. The env file and the git ignore line.
  const envName = basename(envPath);
  const existing = await readEnvFile(envPath);
  const assignments: Record<string, string> = { CETRA_BASE_URL: signedIn.grantServer };
  if (keyReplaceable(existing, signedIn.grantServer)) {
    const replace =
      yesToAll ||
      (interactive() &&
        affirmative(await ask(`Replace the CETRA_API_KEY in ${envName}? [y/N] `, ""), false));
    if (replace) {
      assignments.CETRA_API_KEY = signedIn.key;
    } else {
      io.stderr(`kept the existing CETRA_API_KEY in ${envName}`);
    }
  } else {
    assignments.CETRA_API_KEY = signedIn.key;
  }
  await writeEnvFile(envPath, setVariables(existing, assignments));
  if (await ignoreInGit(dir, envPath)) {
    io.stderr(`Added ${displayPath(dir, envPath)} to .gitignore.`);
  }

  // 6. The project link.
  const linkPath = await writeLink(dir, {
    server,
    org: signedIn.org,
    project: signedIn.project,
    project_id: signedIn.projectId,
  });

  // 7. The skill.
  const skills: string[] = [];
  if (targets.length > 0) {
    const result = await installSkill(
      deps.skillSource ?? bundledSkill(),
      await skillRoot(dir),
      targets,
      VERSION,
    );
    for (const path of result.installed) {
      skills.push(path);
      io.stderr(`Installed the cetra skill in ${path}`);
    }
    for (const skip of result.skipped) io.stderr(`warning: ${skip.path}: ${skip.reason}`);
  }

  // 8. The output.
  const project = `${signedIn.org}/${signedIn.project}`;
  const tracesUrl = `${signedIn.grantServer}/${project}/traces`;
  const prompt = agentPrompt({
    envFile: displayPath(dir, envPath),
    tracesUrl,
    skill: values["no-skill"] !== true,
  });
  io.stderr("");
  io.stderr("Paste this prompt into your coding agent:");
  io.stderr("");
  for (const line of prompt.split("\n")) io.stderr(line);
  io.stderr("");
  io.stderr(`Traces: ${tracesUrl}`);
  if (started !== undefined) {
    io.stderr(`Stop the server: ${started.stop}`);
    io.stderr(`Start it again: ${started.start}`);
    if (started.firstStart && started.email !== undefined && started.password !== undefined) {
      io.stderr(`UI: ${started.server}`);
      io.stderr(`Admin: ${started.email}`);
      if (process.stderr.isTTY === true) {
        io.stderr(`Password: ${started.password}`);
      } else {
        io.stderr(
          `The admin password is in no file; reset it with cetra admin reset-password ${started.email}`,
        );
      }
    }
  }
  if (values.json === true) {
    io.stdout(
      JSON.stringify({
        server: signedIn.grantServer,
        project,
        env_file: envPath,
        link_file: linkPath,
        skills,
        prompt,
        traces_url: tracesUrl,
      }),
    );
  }
  return Exit.Ok;
}

/** The skill targets: `--agents` (a comma list, default both), none with `--no-skill`. */
function skillTargets(list: string | undefined, noSkill: boolean): Target[] {
  if (list !== undefined && noSkill) {
    throw new CliError("--agents and --no-skill cannot be combined", Exit.Usage);
  }
  if (noSkill) return [];
  if (list === undefined) return Object.keys(SKILL_TARGETS).filter(isTarget);
  const picked: Target[] = [];
  for (const name of list.split(",").map((part) => part.trim())) {
    if (!isTarget(name))
      throw new CliError(`unknown agent ${name}; use claude or agents`, Exit.Usage);
    if (!picked.includes(name)) picked.push(name);
  }
  return picked;
}

function isTarget(name: string): name is Target {
  return Object.hasOwn(SKILL_TARGETS, name);
}

/** The project directory: `--dir` relative to the working directory, default the working directory.
 *  It must be an existing directory. */
async function projectDirectory(flag: string | undefined, cwd: string): Promise<string> {
  const dir = resolve(cwd, flag ?? ".");
  const info = await stat(dir).catch(() => undefined);
  if (info === undefined || !info.isDirectory()) {
    throw new CliError("--dir must be an existing directory", Exit.Usage);
  }
  return dir;
}

/** The server URL before any sign-in: `--url`, else `CETRA_URL`, else the project link, else the
 *  cloud. An environment value that is empty or only spaces counts as unset. */
async function chosenServer(
  flag: string | undefined,
  env: NodeJS.ProcessEnv,
  dir: string,
): Promise<string> {
  const given = flag ?? nonEmpty(env.CETRA_URL);
  if (given !== undefined) return given;
  const link = await findLink(dir);
  if (link !== undefined) return linkServer(link);
  return CLOUD;
}

/** The link's server, checked as `serverUrl` checks it; a bad one makes the link invalid. */
function linkServer(link: ProjectLink): string {
  try {
    return serverUrl(link.server);
  } catch {
    throw new CliError(`${LINK_FILE} is invalid`);
  }
}

/** The server URL must be a valid `.env` value, so that nothing fails after the secrets exist. */
function checkEnvValue(server: string): void {
  try {
    setVariables(undefined, { CETRA_BASE_URL: server });
  } catch {
    throw new CliError("the server URL cannot be written to the env file", Exit.Usage);
  }
}

/** Signs in to a local first start with its generated admin password. */
async function signInWithPassword(
  server: string,
  email: string,
  password: string,
): Promise<SignedIn> {
  const local = await passwordSignIn(server, email, password, localHost());
  return {
    grantServer: server,
    token: local.token,
    key: local.key,
    org: local.org,
    project: local.project,
    projectId: local.projectId,
  };
}

/** Signs in with the device flow (`purpose: init`); the grant brings the project and the key. */
async function signInWithDevice(
  server: string,
  project: string | undefined,
  noBrowser: boolean,
  io: Io,
  deps: Deps,
): Promise<SignedIn> {
  const grant = await deviceSignIn({
    server,
    purpose: "init",
    noBrowser,
    project,
    stderr: io.stderr,
    deps,
  });
  const granted = grant.project;
  const key = grant.ingest_key;
  if (granted === undefined || key === undefined) {
    throw new CliError("the server returned an invalid grant", Exit.SignIn);
  }
  return {
    grantServer: grant.server,
    token: grant.token,
    key,
    org: granted.org_slug,
    project: granted.slug,
    projectId: granted.id,
  };
}

/** Whether an existing `CETRA_API_KEY` for the same server may be replaced (the question is asked
 *  by the caller). */
function keyReplaceable(text: string | undefined, server: string): boolean {
  if (text === undefined) return false;
  return (
    readVariable(text, "CETRA_API_KEY") !== undefined &&
    readVariable(text, "CETRA_BASE_URL") === server
  );
}

/** The env file's text, or undefined when it does not exist. Any other read error stops `init`
 *  before the file could be overwritten. */
async function readEnvFile(path: string): Promise<string | undefined> {
  try {
    return await readFile(path, "utf8");
  } catch (error) {
    if (error instanceof Error && "code" in error && error.code === "ENOENT") return undefined;
    throw new CliError(`could not write ${basename(path)}`, Exit.Write);
  }
}

/** A path relative to the project directory, with `/` separators; the absolute path when it lies
 *  outside the directory. */
function displayPath(dir: string, path: string): string {
  const relativePath = relative(dir, path);
  if (relativePath === "" || isAbsolute(relativePath) || relativePath.split(sep)[0] === "..") {
    return path;
  }
  return relativePath.split(sep).join("/");
}

/** An environment value that is empty or only spaces counts as unset. */
function nonEmpty(value: string | undefined): string | undefined {
  return value === undefined || value.trim() === "" ? undefined : value;
}
