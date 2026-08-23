import {
  chmodSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { request as httpRequest } from "node:http";
import { createServer as createHttpsServer } from "node:https";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawn } from "node:child_process";

if (process.platform !== "linux") {
  console.error("Dufs frontend tests support Linux only");
  process.exit(1);
}

const currentDir = dirname(fileURLToPath(import.meta.url));
const projectRoot = resolve(currentDir, "..", "..");
const shareRoot = mkdtempSync(join(tmpdir(), "dufs-frontend-test-"));
const stateRoot = mkdtempSync(join(tmpdir(), "dufs-frontend-state-"));
chmodSync(stateRoot, 0o700);
const externalPort = Number(process.env.DUFS_FRONTEND_TEST_PORT);
if (
  !Number.isSafeInteger(externalPort) ||
  externalPort < 1 ||
  externalPort > 65535
) {
  console.error("DUFS_FRONTEND_TEST_PORT is required");
  process.exit(1);
}
const tlsCert = resolve(
  projectRoot,
  process.env.DUFS_FRONTEND_TEST_CERT || "tests/data/cert.pem",
);
const tlsKey = resolve(
  projectRoot,
  process.env.DUFS_FRONTEND_TEST_KEY || "tests/data/key_pkcs8.pem",
);
const cargoTargetDir = resolve(
  projectRoot,
  process.env.CARGO_TARGET_DIR || "target",
);
const binary = join(cargoTargetDir, "debug", "dufs");
const testPasswordHash =
  "$argon2id$v=19$m=19456,t=2,p=1$HdPI2G8k0h+yEgnqIt2rSw$P+MRyz7wH+b/iPY+He/9DApcy6yB9TAoo7j2JG1Smzs";
const testAccounts = [`frontend-test-0:${testPasswordHash}`];

mkdirSync(join(shareRoot, "existing-folder"));
writeFileSync(join(shareRoot, "existing-folder", "nested.txt"), "nested");
writeFileSync(join(shareRoot, "special & # + 中文.txt"), "special search result");
writeFileSync(
  join(shareRoot, `危险 <img src=x onerror=alert(1)> & "'.txt`),
  "must remain plain text",
);
writeFileSync(join(shareRoot, "download-me.txt"), "downloaded by browser test");
writeFileSync(join(shareRoot, "rename-me.txt"), "rename target");
writeFileSync(join(shareRoot, "overwrite-source.txt"), "replacement content");
writeFileSync(join(shareRoot, "overwrite-target.txt"), "original content");
writeFileSync(join(shareRoot, "delete-me.txt"), "delete target");

const child = spawn(
  binary,
  [
    shareRoot,
    "--bind",
    "127.0.0.1",
    "--trusted-proxy",
    "127.0.0.1/32",
    "--port",
    "0",
    "--min-free-space",
    "0",
    "--state-dir",
    stateRoot,
    ...testAccounts.flatMap(account => ["--auth", account]),
  ],
  {
    cwd: projectRoot,
    stdio: ["ignore", "pipe", "inherit"],
  },
);

let proxyServer;
let stopping = false;
let requestedExitCode = 0;
let startupOutput = "";
const startupTimer = setTimeout(() => {
  console.error("Timed out waiting for the Dufs backend to start");
  stop(1);
}, 15_000);

child.stdout.setEncoding("utf8");
child.stdout.on("data", chunk => {
  process.stdout.write(chunk);
  startupOutput = `${startupOutput}${chunk}`.slice(-4096);
  const match = startupOutput.match(
    /Listening on http:\/\/127\.0\.0\.1:(\d+)/,
  );
  if (match && !proxyServer) {
    clearTimeout(startupTimer);
    startProxy(Number(match[1]));
  }
});

function startProxy(backendPort) {
  proxyServer = createHttpsServer(
    {
      cert: readFileSync(tlsCert),
      key: readFileSync(tlsKey),
    },
    (request, response) => {
      const host = request.headers.host || `127.0.0.1:${externalPort}`;
      const headers = {
        ...request.headers,
        host,
        "x-forwarded-host": host,
        "x-forwarded-for": "127.0.0.1",
        "x-forwarded-proto": "https",
      };
      const upstream = httpRequest({
        hostname: "127.0.0.1",
        port: backendPort,
        method: request.method,
        path: request.url,
        headers,
      }, upstreamResponse => {
        response.writeHead(
          upstreamResponse.statusCode || 502,
          upstreamResponse.headers,
        );
        upstreamResponse.pipe(response);
      });
      upstream.on("error", error => {
        if (!response.headersSent) {
          response.writeHead(502, { "content-type": "text/plain" });
        }
        response.end(`Test gateway error: ${error.message}`);
      });
      request.on("aborted", () => upstream.destroy());
      request.pipe(upstream);
    },
  );
  proxyServer.on("error", error => {
    if (
      error.code === "EADDRINUSE" &&
      process.env.DUFS_FRONTEND_TEST_PORT_ERROR_FILE
    ) {
      writeFileSync(
        process.env.DUFS_FRONTEND_TEST_PORT_ERROR_FILE,
        "port in use",
      );
    }
    console.error(`Unable to start test gateway: ${error.message}`);
    stop(1);
  });
  proxyServer.listen(externalPort, "127.0.0.1");
}

function cleanup() {
  rmSync(shareRoot, { recursive: true, force: true });
  rmSync(stateRoot, { recursive: true, force: true });
}

function stop(exitCode = 0) {
  if (stopping) {
    requestedExitCode = Math.max(requestedExitCode, exitCode);
    return;
  }
  stopping = true;
  requestedExitCode = exitCode;
  clearTimeout(startupTimer);
  proxyServer?.close();
  if (!child.killed) child.kill("SIGTERM");
}

process.on("SIGINT", () => stop());
process.on("SIGTERM", () => stop());

child.on("error", error => {
  console.error(`Unable to start test server: ${error.message}`);
  cleanup();
  process.exit(1);
});

child.on("exit", (code, signal) => {
  clearTimeout(startupTimer);
  proxyServer?.close();
  cleanup();
  if (stopping) {
    process.exit(requestedExitCode);
  }
  console.error(`Test server exited unexpectedly (${signal || code})`);
  process.exit(code || 1);
});
