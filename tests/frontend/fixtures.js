const { randomUUID } = require("node:crypto");
const { test: base, expect } = require("@playwright/test");

const TEST_PASSWORD = "test-password";
const MAX_LOGIN_ATTEMPTS = 4;
const RATE_LIMIT_LOGIN_ERROR =
  "Too many sign-in requests. Please try again later.";
const SEED_DIRECTORIES = ["existing-folder"];
const SEED_FILES = [
  ["existing-folder/nested.txt", "nested"],
  ["special & # + 中文.txt", "special search result"],
  [`危险 <img src=x onerror=alert(1)> & "'.txt`, "must remain plain text"],
  ["download-me.txt", "downloaded by browser test"],
  ["rename-me.txt", "rename target"],
  ["overwrite-source.txt", "replacement content"],
  ["overwrite-target.txt", "original content"],
  ["delete-me.txt", "delete target"],
];

function isRelevantCspViolation(text) {
  if (!/content[- ]security[- ]policy/i.test(text)) return false;
  return !(/FaviconLoader\.sys\.mjs/i.test(text) && /\/favicon\.ico\b/i.test(text));
}

function testUsername(parallelIndex = 0) {
  if (parallelIndex !== 0) {
    throw new Error(`Unsupported Playwright parallel index: ${parallelIndex}`);
  }
  return "frontend-test-0";
}

async function login(page, parallelIndex = 0) {
  await page.goto("/");
  await expect(page).toHaveURL(/\/__dufs__\/login$/);
  await submitKnownLogin(page, testUsername(parallelIndex));
  await expect(page.locator(".index-page")).toBeVisible();
  // Wait for the asynchronous first page without depending on any particular
  // entry in the shared root.
  await expect(page.locator(".list-status")).toContainText(
    /\b\d+ items loaded\b/,
  );
}

async function submitKnownLogin(page, username) {
  for (let attempt = 1; attempt <= MAX_LOGIN_ATTEMPTS; attempt++) {
    await page.getByLabel("Username").fill(username);
    await page.getByLabel("Password").fill(TEST_PASSWORD);
    const [response] = await Promise.all([
      page.waitForNavigation({ waitUntil: "load" }),
      page.getByRole("button", { name: "Sign in" }).click(),
    ]);
    if (new URL(page.url()).pathname === "/") return;

    const message = (await page.getByRole("alert").textContent())?.trim() || "";
    if (response?.status() !== 429 || message !== RATE_LIMIT_LOGIN_ERROR) {
      throw new Error(`Test sign-in failed: ${message || `HTTP ${response?.status() || 0}`}`);
    }
    if (attempt === MAX_LOGIN_ATTEMPTS) {
      throw new Error(
        `Test sign-in remained rate limited after ${MAX_LOGIN_ATTEMPTS} attempts`,
      );
    }
    const retryAfter = Number(response.headers()["retry-after"]);
    if (!Number.isSafeInteger(retryAfter) || retryAfter < 1) {
      throw new Error("Rate-limited test sign-in returned invalid Retry-After");
    }
    await page.waitForTimeout(retryAfter * 1000);
  }
}

const test = base.extend({
  axePage: async ({ browser }, use, testInfo) => {
    const context = await browser.newContext({
      baseURL: testInfo.project.use.baseURL,
      bypassCSP: true,
      ignoreHTTPSErrors: true,
      viewport: { width: 1280, height: 800 },
    });
    const page = await context.newPage();
    await use(page);
    await context.close();
  },
  appPage: async ({ page }, use, testInfo) => {
    const pageErrors = [];
    const cspViolations = [];
    page.on("pageerror", error => pageErrors.push(error.message));
    page.on("console", message => {
      const text = message.text();
      if (isRelevantCspViolation(text)) cspViolations.push(text);
    });
    await login(page, testInfo.parallelIndex);
    const testRoot = `/pw-${testInfo.parallelIndex}-${randomUUID()}`;
    await seedWorkspace(page, testRoot);
    await page.goto(`${testRoot}/`);
    await expect(page.locator(".index-page")).toBeVisible();
    await expect(
      page.getByRole("link", { name: "existing-folder", exact: true }),
    ).toBeVisible();
    await use(page);
    expect(pageErrors).toEqual([]);
    expect(cspViolations).toEqual([]);
  },
});

async function rotateSession(page) {
  const username = (await pageData(page)).user;
  const relogin = await page.context().newPage();
  await relogin.goto("/__dufs__/login");
  await submitKnownLogin(relogin, username);
  await relogin.close();
}

async function seedWorkspace(page, root) {
  const data = await pageData(page);
  const request = page.context().request;
  const apiUrl = new URL("/__dufs__/api/mkdir", page.url()).href;
  for (const path of [root, ...SEED_DIRECTORIES.map(name => `${root}/${name}`)]) {
    const operationId = randomUUID();
    const response = await request.post(apiUrl, {
      // Playwright only retries ECONNRESET. Reusing this operation ID makes the
      // single retry safe whether the first response was lost before or after
      // the server committed the mkdir operation.
      maxRetries: 1,
      headers: {
        "Content-Type": "application/json",
        "X-Dufs-CSRF-Token": data.csrf_token,
        "X-Dufs-Operation-Id": operationId,
      },
      data: { path },
    });
    if (response.status() !== 201) {
      throw new Error(
        `Unable to seed test directory ${path}: HTTP ${response.status()} ${await response.text()}`,
      );
    }
  }
  for (const [name, contents] of SEED_FILES) {
    const body = Buffer.from(contents);
    const response = await request.put(logicalPathUrl(page, `${root}/${name}`), {
      headers: {
        "X-Dufs-CSRF-Token": data.csrf_token,
        "X-Dufs-Upload-Id": randomUUID(),
        "X-Dufs-Upload-Length": String(body.length),
      },
      data: body,
    });
    if (response.status() !== 201) {
      throw new Error(
        `Unable to seed test file ${name}: HTTP ${response.status()} ${await response.text()}`,
      );
    }
  }
}

function rowByName(page, name) {
  return page
    .locator(".paths-table tbody tr")
    .filter({ has: page.getByRole("link", { name, exact: true }) });
}

function actionDialog(page, title) {
  return page.getByRole("dialog", { name: title, exact: true });
}

async function submitActionDialog(page, options) {
  const dialog = actionDialog(page, options.title);
  await expect(dialog).toBeVisible();
  if (options.value !== undefined) {
    const input = dialog.getByRole("textbox", { name: options.label });
    await expect(input).toBeFocused();
    await input.fill(options.value);
  }
  await dialog.getByRole("button", {
    name: options.confirmText,
    exact: true,
  }).click();
}

async function selectFiles(page, selector, files) {
  await page.locator(selector).evaluate((input, payloads) => {
    const transfer = new DataTransfer();
    for (const payload of payloads) {
      const binary = atob(payload.base64);
      const bytes = Uint8Array.from(binary, character => character.charCodeAt(0));
      const file = new File([bytes], payload.name, {
        type: payload.mimeType,
        lastModified: payload.lastModified,
      });
      if (payload.relativePath) {
        Object.defineProperty(file, "webkitRelativePath", {
          value: payload.relativePath,
        });
      }
      transfer.items.add(file);
    }
    input.files = transfer.files;
    input.dispatchEvent(new Event("change", { bubbles: true }));
  }, files.map(file => ({
    name: file.name,
    mimeType: file.mimeType || "application/octet-stream",
    lastModified: file.lastModified || 1_722_000_000_000,
    relativePath: file.relativePath,
    base64: Buffer.from(file.buffer).toString("base64"),
  })));
}

async function pageData(page) {
  const encoded = await page.locator("#index-data").evaluate(
    template => template.content.textContent,
  );
  return JSON.parse(Buffer.from(encoded, "base64").toString("utf8"));
}

function currentDirectoryPath(page) {
  const path = decodeURIComponent(new URL(page.url()).pathname)
    .replace(/\/+$/, "");
  return path || "/";
}

function currentLogicalChild(page, name) {
  const root = currentDirectoryPath(page);
  return `${root === "/" ? "" : root}/${name}`;
}

function currentUrl(page, name) {
  const base = new URL(page.url());
  base.search = "";
  base.hash = "";
  if (!base.pathname.endsWith("/")) base.pathname += "/";
  const encoded = name
    .split("/")
    .map(part => encodeURIComponent(part))
    .join("/");
  return new URL(encoded, base).href;
}

function logicalPathUrl(page, path) {
  const encoded = path
    .split("/")
    .map(part => encodeURIComponent(part))
    .join("/");
  return new URL(encoded, new URL("/", page.url())).href;
}

module.exports = {
  actionDialog,
  currentDirectoryPath,
  currentLogicalChild,
  currentUrl,
  expect,
  login,
  pageData,
  rotateSession,
  rowByName,
  selectFiles,
  submitActionDialog,
  test,
};
