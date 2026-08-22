import { createServer } from "node:http";

const [socketPath] = process.argv.slice(2);
if (!socketPath) {
  throw new Error("Usage: mock-upstream.mjs <unix-socket>");
}

const server = createServer((request, response) => {
  const chunks = [];
  request.on("data", chunk => chunks.push(chunk));
  request.on("end", () => {
    const finish = () => {
      response.writeHead(200, {
        "content-type": "application/json",
        "cache-control": "no-store",
      });
      response.end(JSON.stringify({
        body_bytes: chunks.reduce((total, chunk) => total + chunk.length, 0),
        connection: request.headers.connection || "",
        host: request.headers.host || "",
        http_version: request.httpVersion,
        method: request.method,
        url: request.url,
        x_forwarded_for: request.headers["x-forwarded-for"] || "",
        x_forwarded_host: request.headers["x-forwarded-host"] || "",
        x_forwarded_proto: request.headers["x-forwarded-proto"] || "",
      }));
    };
    if (request.url?.includes("/hold")) {
      setTimeout(finish, 1_000);
    } else {
      finish();
    }
  });
});

server.on("error", error => {
  console.error(error);
  process.exitCode = 1;
});
server.listen(socketPath, () => {
  process.stdout.write("ready\n");
});

function stop() {
  server.close(() => process.exit());
}
process.once("SIGINT", stop);
process.once("SIGTERM", stop);
