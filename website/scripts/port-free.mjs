import { createServer } from "node:net";

// True when nothing listens on `port`: binding every address of the host fails with
// EADDRINUSE when any other process holds the port, on any of its addresses.
export function portIsFree(port) {
  return new Promise((resolve, reject) => {
    const server = createServer();
    server.once("error", (error) => {
      if (error.code === "EADDRINUSE") resolve(false);
      else reject(error);
    });
    server.listen(port, () => server.close(() => resolve(true)));
  });
}
