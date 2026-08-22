import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { createRequire } from "node:module";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";

const require = createRequire(import.meta.url);
const playwrightCli = require.resolve("@playwright/test/cli");
const rawArgs = process.argv.slice(2);
const forwardedArgs = [];
const requestedProjects = [];
let edgeOnly = false;

for (let index = 0; index < rawArgs.length; index++) {
  const argument = rawArgs[index];
  if (argument === "--edge") {
    edgeOnly = true;
  } else if (argument === "--project") {
    const project = rawArgs[++index];
    if (!project) throw new Error("--project requires a value");
    requestedProjects.push(project);
  } else if (argument.startsWith("--project=")) {
    requestedProjects.push(argument.slice("--project=".length));
  } else {
    forwardedArgs.push(argument);
  }
}

const projects = requestedProjects.length > 0
  ? [...new Set(requestedProjects)]
  : edgeOnly
    ? ["edge"]
    : ["chromium", "firefox"];
for (const project of projects) {
  if (!["chromium", "firefox", "edge"].includes(project)) {
    throw new Error(`不支持的浏览器测试项目：${project}`);
  }
}

function allocatePort() {
  return new Promise((resolve, reject) => {
    const server = createServer();
    server.unref();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      const port = typeof address === "object" && address
        ? address.port
        : 0;
      server.close(error => error ? reject(error) : resolve(port));
    });
  });
}

let activeChild;
let forwardedSignal;
for (const signal of ["SIGINT", "SIGTERM"]) {
  process.once(signal, () => {
    forwardedSignal = signal;
    activeChild?.kill(signal);
  });
}

for (const project of projects) {
  if (forwardedSignal) break;
  const signalDirectory = mkdtempSync(
    join(tmpdir(), "dufs-frontend-runner-"),
  );
  const portErrorFile = join(signalDirectory, "port-in-use");
  let code = 1;
  for (let attempt = 0; attempt < 3; attempt++) {
    const port = await allocatePort();
    code = await runProject(project, port, portErrorFile);
    if (!existsSync(portErrorFile)) break;
    rmSync(portErrorFile, { force: true });
  }
  rmSync(signalDirectory, { recursive: true, force: true });
  if (code !== 0) {
    process.exitCode = code;
    break;
  }
}

if (forwardedSignal) process.kill(process.pid, forwardedSignal);

function runProject(project, port, portErrorFile) {
  return new Promise((resolve, reject) => {
    activeChild = spawn(
      process.execPath,
      [playwrightCli, "test", ...forwardedArgs],
      {
        stdio: "inherit",
        env: {
          ...process.env,
          DUFS_FRONTEND_TEST_PORT: String(port),
          DUFS_FRONTEND_TEST_PORT_ERROR_FILE: portErrorFile,
          DUFS_FRONTEND_TEST_PROJECT: project,
        },
      },
    );
    activeChild.once("error", reject);
    activeChild.once("exit", (code, signal) => {
      activeChild = undefined;
      if (signal) {
        forwardedSignal = signal;
        resolve(1);
      } else {
        resolve(code ?? 1);
      }
    });
  });
}
