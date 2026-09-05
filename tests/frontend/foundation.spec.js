const { readFileSync } = require("node:fs");
const AxeBuilder = require("@axe-core/playwright").default;
const { test, expect, pageData, login } = require("./fixtures.js");

test("原生 Profile 的实际嵌入字体、许可证和恢复会话来自 Foundation", async ({ appPage: page }) => {
  await pageData(page);
  const platformCss = page.locator('link[rel="stylesheet"][href$="/dist/platform.css"]');
  const prefix = new URL("./", await platformCss.evaluate(link => link.href));
  for (const name of ["MapleMono.woff2", "MapleMono-Italic.woff2", "OFL.txt"]) {
    const response = await page.context().request.get(new URL(name, prefix).href);
    expect(response.status()).toBe(200);
    expect(response.headers()["cache-control"]).toContain("immutable");
    expect(response.headers()["x-content-type-options"]).toBe("nosniff");
    expect(response.headers()["content-type"]).toContain(name.endsWith("woff2") ? "font/woff2" : "text/plain");
    expect(await response.body()).toEqual(readFileSync(require.resolve(`@sarmg/web-fonts/${name}`)));
  }
  await page.evaluate(() => document.fonts.ready);
  expect(await page.evaluate(() => document.fonts.check('16px "Sarmg Maple"'))).toBe(true);
  expect(await page.locator("body").evaluate(element => getComputedStyle(element).fontFamily)).toContain("Sarmg Maple");
  const script = await page.context().request.get(new URL("platform.js", prefix).href);
  expect((await script.body()).byteLength).toBeLessThanOrEqual(256 * 1024);
  expect(await script.text()).not.toMatch(/react-dom|react\/jsx-runtime|sourceMappingURL/u);
});

test("原生 Profile 的移动明暗主题通过 WCAG AA", async ({ axePage: page }) => {
  await login(page);
  await page.setViewportSize({ width: 390, height: 844 });
  for (const colorScheme of ["light", "dark"]) {
    await page.emulateMedia({ colorScheme, reducedMotion: "reduce" });
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
    const results = await new AxeBuilder({ page }).withTags(["wcag2a", "wcag2aa", "wcag21a", "wcag21aa"]).analyze();
    expect(results.violations).toEqual([]);
  }
});
