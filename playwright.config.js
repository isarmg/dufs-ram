const { defineConfig } = require("@playwright/test");

function requiredPort(name) {
  const port = Number(process.env[name]);
  if (!Number.isSafeInteger(port) || port < 1 || port > 65535) {
    throw new Error(`请通过 tests/frontend/run.mjs 分配 ${name}`);
  }
  return port;
}

const port = requiredPort("DUFS_FRONTEND_TEST_PORT");
const projectName = process.env.DUFS_FRONTEND_TEST_PROJECT;
if (!["chromium", "firefox", "edge"].includes(projectName)) {
  throw new Error("请通过 tests/frontend/run.mjs 选择浏览器测试项目");
}

const browser = projectName === "edge"
  ? { browserName: "chromium", channel: "msedge" }
  : { browserName: projectName };
const baseURL = `https://127.0.0.1:${port}`;

module.exports = defineConfig({
  testDir: "./tests/frontend",
  testIgnore: "unit/**",
  // The isolated gateway deliberately presents one client address to Dufs.
  // Keep UI cases serial so unrelated test logins cannot contend for the
  // production global/source token buckets and become scheduler-dependent.
  workers: 1,
  retries: 1,
  failOnFlakyTests: true,
  timeout: 30_000,
  expect: {
    timeout: 8_000,
  },
  reporter: [["line"]],
  use: {
    ignoreHTTPSErrors: true,
    viewport: {
      width: 1280,
      height: 800,
    },
    trace: "retain-on-failure",
  },
  projects: [{
    name: projectName,
    use: {
      ...browser,
      baseURL,
    },
  }],
  webServer: {
    command: "node tests/frontend/server.mjs",
    url: `${baseURL}/__dufs__/login`,
    env: {
      DUFS_FRONTEND_TEST_PORT: String(port),
    },
    reuseExistingServer: false,
    timeout: 120_000,
    ignoreHTTPSErrors: true,
  },
});
