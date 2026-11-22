import { optionsHelp, parseOptions } from "./args.js";
import {
  chosenServer,
  credentialsPath,
  loadCredentials,
  saveToken,
  serverUrl,
} from "./credentials.js";
import { deviceSignIn } from "./device.js";
import { CliError, type Deps, Exit, type ExitCode, type Io } from "./errors.js";
import { requestJson } from "./http.js";

const SUMMARY = "Sign in again and save a new token.";
const LINES: [string, string][] = [
  ["--url <URL>", "The server. Defaults to CETRA_URL, then the only saved server"],
  ["--no-browser", "Print the sign-in URL and do not open a browser"],
  ["--json", 'Print {"server": ...} on stdout'],
  ["-h, --help", "Show this help"],
];

/** `cetra-cli login`: the device flow, a check of the token with `/api/auth/me`, then the token
 *  is saved in the shared credentials file. Human text goes to stderr. */
export async function login(args: string[], io: Io, deps: Deps = {}): Promise<ExitCode> {
  const { values, positionals } = parseOptions(args, {
    url: { type: "string" },
    "no-browser": { type: "boolean" },
    json: { type: "boolean" },
    help: { type: "boolean", short: "h" },
  });
  if (values.help === true) {
    io.stderr(optionsHelp("login", SUMMARY, LINES));
    return Exit.Ok;
  }
  if (positionals.length > 0) {
    throw new CliError("login takes no positional arguments", Exit.Usage);
  }

  const credentials = credentialsPath(io.env);
  const flag = values.url ?? nonEmpty(io.env.CETRA_URL);
  const server =
    flag !== undefined ? serverUrl(flag) : chosenServer(await loadCredentials(credentials));

  // The server that answered the grant: the typed one, or the cell a `moved` answer named. The
  // token is checked and saved there only, never sent to the server that answered `moved`.
  const { grant, server: answering } = await deviceSignIn({
    server,
    purpose: "login",
    noBrowser: values["no-browser"] === true,
    stderr: io.stderr,
    deps,
  });
  const me = await requestJson("GET", `${answering}/api/auth/me`, { token: grant.token });
  if (me.status < 200 || me.status > 299) {
    throw new CliError("the server rejected the token", Exit.SignIn);
  }
  await saveToken(credentials, answering, grant.token);
  io.stderr(`Signed in to ${answering}. The token expires in 90 days.`);
  if (values.json === true) io.stdout(JSON.stringify({ server: answering }));
  return Exit.Ok;
}

/** An environment value that is empty or only spaces counts as unset. */
function nonEmpty(value: string | undefined): string | undefined {
  return value === undefined || value.trim() === "" ? undefined : value;
}
