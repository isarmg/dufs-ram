const { test: base, expect } = require("@playwright/test");

function isRelevantCspViolation(text) {
  if (!/content[- ]security[- ]policy/i.test(text)) return false;
  return !(/FaviconLoader\.sys\.mjs/i.test(text) && /\/favicon\.ico\b/i.test(text));
}

async function login(page) {
  await page.goto("/");
  await expect(page).toHaveURL(/\/__dufs__\/login$/);
  await page.getByLabel("账号").fill("frontend-test");
  await page.getByLabel("密码").fill("test-password");
  await Promise.all([
    page.waitForURL(url => url.pathname === "/"),
    page.getByRole("button", { name: "登录" }).click(),
  ]);
  await expect(page.locator(".index-page")).toBeVisible();
  await expect(
    page.getByRole("link", { name: "existing-folder", exact: true }),
  ).toBeVisible();
}

const test = base.extend({
  appPage: async ({ page }, use) => {
    const pageErrors = [];
    const cspViolations = [];
    page.on("pageerror", error => pageErrors.push(error.message));
    page.on("console", message => {
      const text = message.text();
      if (isRelevantCspViolation(text)) cspViolations.push(text);
    });
    await login(page);
    await use(page);
    expect(pageErrors).toEqual([]);
    expect(cspViolations).toEqual([]);
  },
});

async function rotateSession(page) {
  const relogin = await page.context().newPage();
  await relogin.goto("/__dufs__/login");
  await relogin.getByLabel("账号").fill("frontend-test");
  await relogin.getByLabel("密码").fill("test-password");
  await Promise.all([
    relogin.waitForURL(url => url.pathname === "/"),
    relogin.getByRole("button", { name: "登录" }).click(),
  ]);
  await relogin.close();
}

function rowByName(page, name) {
  return page
    .locator(".paths-table tbody tr")
    .filter({ has: page.getByRole("link", { name, exact: true }) });
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

module.exports = {
  expect,
  login,
  pageData,
  rotateSession,
  rowByName,
  selectFiles,
  test,
};
