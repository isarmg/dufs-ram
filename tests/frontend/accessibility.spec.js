const { readFileSync, readdirSync } = require("node:fs");
const { join, resolve } = require("node:path");
const { expect, rowByName, test } = require("./fixtures");

test("主要文件管理控件使用原生语义和键盘操作", async ({ appPage: page }) => {
  for (const name of ["上传文件", "上传文件夹", "新建文件夹", "新建空文件", "退出登录"]) {
    const control = page.getByRole("button", { name, exact: true });
    await expect(control).toBeVisible();
    expect(await control.evaluate(element => element.tagName)).toBe("BUTTON");
  }

  const fileChooserPromise = page.waitForEvent("filechooser");
  await page.getByRole("button", { name: "上传文件", exact: true }).focus();
  await page.keyboard.press("Enter");
  const chooser = await fileChooserPromise;
  await chooser.setFiles([]);
  await expect(
    page.getByRole("button", { name: "上传文件", exact: true }),
  ).toBeFocused();

  page.once("dialog", dialog => dialog.dismiss());
  const newFolder = page.getByRole("button", { name: "新建文件夹" });
  await newFolder.focus();
  await page.keyboard.press("Space");
  await expect(newFolder).toBeFocused();

  const row = rowByName(page, "download-me.txt");
  const move = row.getByRole("button", { name: "移动或重命名 download-me.txt" });
  const remove = row.getByRole("button", { name: "删除 download-me.txt" });
  expect(await move.evaluate(element => element.tagName)).toBe("BUTTON");
  expect(await remove.evaluate(element => element.tagName)).toBe("BUTTON");
  await expect(page.locator(".upload-queue-message")).toHaveAttribute(
    "aria-live",
    "assertive",
  );

  await expect(page.locator("#file")).toHaveAttribute("tabindex", "-1");
  await expect(page.locator("#folder")).toHaveAttribute("tabindex", "-1");
  const search = page.getByLabel("搜索文件或文件夹");
  await search.focus();
  expect(
    await page.locator(".searchbar").evaluate(element => {
      const style = getComputedStyle(element);
      return style.outlineStyle !== "none" &&
        Number.parseFloat(style.outlineWidth) >= 2;
    }),
  ).toBe(true);

  for (const control of [
    page.getByRole("button", { name: "上传文件", exact: true }),
    move,
    remove,
  ]) {
    const box = await control.boundingBox();
    expect(box.width).toBeGreaterThan(23.9);
    expect(box.height).toBeGreaterThan(23.9);
  }
});

test("生产前端源码不包含动态 HTML 注入接口", async () => {
  const modulesDir = resolve(__dirname, "../../assets/modules");
  const files = [
    resolve(__dirname, "../../assets/index.js"),
    ...readdirSync(modulesDir)
      .filter(name => name.endsWith(".js"))
      .map(name => join(modulesDir, name)),
  ];
  const forbidden = /\b(?:innerHTML|outerHTML|insertAdjacentHTML|document\.write|DOMParser)\b/;
  for (const file of files) {
    expect(readFileSync(file, "utf8"), file).not.toMatch(forbidden);
  }
});
