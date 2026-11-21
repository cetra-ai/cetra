import { spawn } from "node:child_process";
import type { Deps } from "./errors.js";

/** Opens `url` in the browser, detached and without a shell. A missing opener program is harmless
 *  (the error listener), and on Linux without a display nothing is started. */
export function openBrowser(
  url: string,
  deps: Deps = {},
  env: NodeJS.ProcessEnv = process.env,
  platform: NodeJS.Platform = process.platform,
): void {
  if (platform === "linux" && !env.DISPLAY && !env.WAYLAND_DISPLAY) return;
  const launch = deps.spawn ?? spawn;
  let child: ReturnType<typeof spawn>;
  if (platform === "darwin") {
    child = launch("open", [url], { detached: true, stdio: "ignore" });
  } else if (platform === "win32") {
    // No shell: `cmd /c start` would read `&` and `|` in the URL as commands.
    child = launch("rundll32", ["url.dll,FileProtocolHandler", url], {
      detached: true,
      stdio: "ignore",
    });
  } else {
    child = launch("xdg-open", [url], { detached: true, stdio: "ignore" });
  }
  child.on("error", () => {});
  child.unref();
}
