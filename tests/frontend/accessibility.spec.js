const { readFileSync, readdirSync } = require("node:fs");
const { join, resolve } = require("node:path");
const AxeBuilder = require("@axe-core/playwright").default;
const {
  actionDialog,
  expect,
  login,
  rowByName,
  test,
} = require("./fixtures");

const axeTags = [
  "wcag2a",
  "wcag2aa",
  "wcag21a",
  "wcag21aa",
  "wcag22a",
  "wcag22aa",
];

function axe(page) {
  return new AxeBuilder({ page }).withTags(axeTags);
}

function walkJavaScript(root) {
  return readdirSync(root, { withFileTypes: true }).flatMap(entry => {
    const path = join(root, entry.name);
    if (entry.isDirectory()) return walkJavaScript(path);
    return entry.isFile() && entry.name.endsWith(".js") ? [path] : [];
  });
}

async function describeActionSlots(slots) {
  return slots.evaluateAll(elements => elements.map(slot => {
    const control = slot.querySelector("a, button");
    return {
      slot: slot.getAttribute("data-action-slot"),
      tag: control?.tagName || null,
      action: control?.getAttribute("data-action") || null,
      label: control?.getAttribute("aria-label") || null,
    };
  }));
}

async function readActionLayout(row) {
  return row.locator(".action-slots").evaluate(grid => {
    const gridRect = grid.getBoundingClientRect();
    return {
      gridLeft: gridRect.left,
      gridRight: gridRect.right,
      slots: [...grid.querySelectorAll(":scope > .action-slot")].map(slot => {
        const rect = slot.getBoundingClientRect();
        return {
          left: rect.left,
          right: rect.right,
          top: rect.top,
          width: rect.width,
          height: rect.height,
        };
      }),
    };
  });
}

async function assertFixedActionColumns(fileRow, directoryRow) {
  const file = await readActionLayout(fileRow);
  const directory = await readActionLayout(directoryRow);

  for (const layout of [file, directory]) {
    expect(layout.slots).toHaveLength(4);
    const firstTop = layout.slots[0].top;
    for (const [index, slot] of layout.slots.entries()) {
      expect(slot.width).toBeGreaterThan(0);
      expect(slot.height).toBeGreaterThan(0);
      expect(Math.abs(slot.top - firstTop)).toBeLessThan(0.5);
      expect(slot.left).toBeGreaterThanOrEqual(layout.gridLeft - 0.5);
      expect(slot.right).toBeLessThanOrEqual(layout.gridRight + 0.5);
      if (index > 0) {
        expect(slot.left).toBeGreaterThan(layout.slots[index - 1].left);
      }
    }
  }

  for (let index = 0; index < 4; index++) {
    expect(
      Math.abs(file.slots[index].left - directory.slots[index].left),
    ).toBeLessThan(0.5);
    expect(
      Math.abs(file.slots[index].width - directory.slots[index].width),
    ).toBeLessThan(0.5);
  }
}

test("主要文件管理控件使用原生语义和键盘操作", async ({ appPage: page }) => {
  const root = page.getByRole("link", { name: "Root", exact: true });
  await expect(root).toBeVisible();
  await expect(root).toHaveAttribute("href", "/");
  await expect(root).toHaveAttribute("title", "Root");
  await expect(root).toHaveText("");
  const rootIcon = root.locator("svg");
  await expect(rootIcon).toHaveCount(1);
  await expect(rootIcon).toHaveAttribute("aria-hidden", "true");

  for (
    const name of [
      "Upload files",
      "Upload folder",
      "New folder",
      "New empty file",
      "Sign out",
    ]
  ) {
    const control = page.getByRole("button", { name, exact: true });
    await expect(control).toBeVisible();
    expect(await control.evaluate(element => element.tagName)).toBe("BUTTON");
  }

  const fileChooserPromise = page.waitForEvent("filechooser");
  await page.getByRole("button", { name: "Upload files", exact: true }).focus();
  await page.keyboard.press("Enter");
  const chooser = await fileChooserPromise;
  await chooser.setFiles([]);
  await expect(
    page.getByRole("button", { name: "Upload files", exact: true }),
  ).toBeFocused();

  const newFolder = page.getByRole("button", { name: "New folder" });
  await newFolder.focus();
  await page.keyboard.press("Space");
  const folderName = page.locator(".inline-name-input");
  await expect(folderName).toHaveCount(1);
  await expect(folderName).toBeFocused();
  await expect(folderName).toHaveValue("newfolder");
  await expect(folderName).toHaveAttribute("aria-label", /newfolder/i);
  await page.keyboard.press("Escape");
  await expect(folderName).toHaveCount(0);
  await expect(
    page.getByRole("link", { name: "newfolder", exact: true }),
  ).toBeVisible();
  await expect(newFolder).toBeFocused();

  const newFile = page.getByRole("button", { name: "New empty file" });
  await newFile.focus();
  await page.keyboard.press("Enter");
  const fileName = page.locator(".inline-name-input");
  await expect(fileName).toBeFocused();
  await expect(fileName).toHaveValue("newfile");
  await expect(fileName).toHaveAttribute("aria-label", /newfile/i);
  await page.keyboard.press("Escape");
  await expect(fileName).toHaveCount(0);
  await expect(newFile).toBeFocused();

  const row = rowByName(page, "download-me.txt");
  const rename = row.getByRole("button", { name: "Rename download-me.txt" });
  const move = row.getByRole("button", { name: "Move download-me.txt" });
  const remove = row.getByRole("button", { name: "Delete download-me.txt" });
  expect(await rename.evaluate(element => element.tagName)).toBe("BUTTON");
  expect(await move.evaluate(element => element.tagName)).toBe("BUTTON");
  expect(await remove.evaluate(element => element.tagName)).toBe("BUTTON");
  await rename.focus();
  await page.keyboard.press("Enter");
  const renameInput = page.locator(".inline-name-input");
  await expect(renameInput).toBeFocused();
  await expect(renameInput).toHaveValue("download-me.txt");
  await expect(renameInput).toHaveAttribute("aria-label", /download-me\.txt/i);
  await page.keyboard.press("Escape");
  await expect(renameInput).toHaveCount(0);
  await expect(rename).toBeFocused();
  await remove.focus();
  await page.keyboard.press("Enter");
  const deleteDialog = actionDialog(page, "Delete item");
  await expect(deleteDialog).toContainText(
    'Delete "download-me.txt"? This action cannot be undone.',
  );
  await expect(deleteDialog).toHaveAttribute(
    "aria-describedby",
    "action-dialog-message",
  );
  await expect(deleteDialog).toHaveAccessibleDescription(
    'Delete "download-me.txt"? This action cannot be undone.',
  );
  const confirmDelete = deleteDialog.getByRole("button", { name: "Delete" });
  await expect(confirmDelete).toBeFocused();
  await page.keyboard.press("Escape");
  await expect(remove).toBeFocused();
  await expect(page.locator(".upload-queue-message")).toHaveAttribute(
    "aria-live",
    "assertive",
  );

  await expect(page.locator("#file")).toHaveAttribute("tabindex", "-1");
  await expect(page.locator("#folder")).toHaveAttribute("tabindex", "-1");
  const search = page.getByLabel("Search files or folders");
  await search.focus();
  expect(
    await page.locator(".searchbar").evaluate(element => {
      const style = getComputedStyle(element);
      return style.outlineStyle !== "none" &&
        Number.parseFloat(style.outlineWidth) >= 2;
    }),
  ).toBe(true);

  for (const control of [
    page.getByRole("button", { name: "Upload files", exact: true }),
    rename,
    move,
    remove,
  ]) {
    const box = await control.boundingBox();
    expect(box.width).toBeGreaterThan(23.9);
    expect(box.height).toBeGreaterThan(23.9);
  }
});

test("操作区固定为 Move、Download、Delete、Rename 四列", async ({
  appPage: page,
}) => {
  const fileRow = rowByName(page, "download-me.txt");
  const directoryRow = rowByName(page, "existing-folder");
  const fileSlots = fileRow.locator(".action-slots > .action-slot");
  const directorySlots = directoryRow.locator(
    ".action-slots > .action-slot",
  );
  const expectedOrder = ["move", "download", "delete", "rename"];

  await expect(fileSlots).toHaveCount(4);
  await expect(directorySlots).toHaveCount(4);
  expect(
    await fileSlots.evaluateAll(slots =>
      slots.map(slot => slot.getAttribute("data-action-slot"))
    ),
  ).toEqual(expectedOrder);
  expect(
    await directorySlots.evaluateAll(slots =>
      slots.map(slot => slot.getAttribute("data-action-slot"))
    ),
  ).toEqual(expectedOrder);

  expect(await describeActionSlots(fileSlots)).toEqual([
    {
      slot: "move",
      tag: "BUTTON",
      action: "move",
      label: "Move download-me.txt",
    },
    {
      slot: "download",
      tag: "A",
      action: null,
      label: "Download file download-me.txt",
    },
    {
      slot: "delete",
      tag: "BUTTON",
      action: "delete",
      label: "Delete download-me.txt",
    },
    {
      slot: "rename",
      tag: "BUTTON",
      action: "rename",
      label: "Rename download-me.txt",
    },
  ]);
  expect(await describeActionSlots(directorySlots)).toEqual([
    {
      slot: "move",
      tag: "BUTTON",
      action: "move",
      label: "Move existing-folder",
    },
    {
      slot: "download",
      tag: null,
      action: null,
      label: null,
    },
    {
      slot: "delete",
      tag: "BUTTON",
      action: "delete",
      label: "Delete existing-folder",
    },
    {
      slot: "rename",
      tag: "BUTTON",
      action: "rename",
      label: "Rename existing-folder",
    },
  ]);

  const directoryDownloadSlot = directorySlots.nth(1);
  expect(
    await directoryDownloadSlot.evaluate(slot => {
      const interactiveSelector =
        "a, button, input, select, textarea, [tabindex], [role]";
      return {
        ariaHidden: slot.getAttribute("aria-hidden"),
        ariaLabel: slot.getAttribute("aria-label"),
        role: slot.getAttribute("role"),
        tabIndexAttribute: slot.getAttribute("tabindex"),
        tabIndex: slot.tabIndex,
        text: slot.textContent?.trim() || "",
        hasInteractiveNode: slot.matches(interactiveSelector) ||
          slot.querySelector(interactiveSelector) !== null,
      };
    }),
  ).toEqual({
    ariaHidden: "true",
    ariaLabel: null,
    role: null,
    tabIndexAttribute: null,
    tabIndex: -1,
    text: "",
    hasInteractiveNode: false,
  });

  await assertFixedActionColumns(fileRow, directoryRow);
  await page.setViewportSize({ width: 320, height: 800 });
  await assertFixedActionColumns(fileRow, directoryRow);
});

test("1280px 桌面在 400% 缩放时可在 320 CSS 像素内回流", async ({
  appPage: page,
}) => {
  await page.setViewportSize({ width: 320, height: 800 });
  await expect(page.getByRole("button", { name: "Upload files" })).toBeVisible();
  await expect(page.getByLabel("Search files or folders")).toBeVisible();
  await expect(page.getByRole("button", { name: "Sign out" })).toBeVisible();

  const row = rowByName(page, "download-me.txt");
  await expect(row.locator(".cell-mtime")).toBeVisible();
  await expect(row.locator(".cell-size")).toBeVisible();
  await expect(row.getByRole("button", {
    name: "Rename download-me.txt",
  })).toBeVisible();
  await expect(row.getByRole("button", {
    name: "Move download-me.txt",
  })).toBeVisible();
  await expect(row.getByRole("button", {
    name: "Delete download-me.txt",
  })).toBeVisible();

  await row.getByRole("button", {
    name: "Rename download-me.txt",
  }).click();
  const inlineEditor = page.locator(".inline-name-input");
  await expect(inlineEditor).toBeFocused();
  await expect(page.locator(".inline-name-marker")).toHaveCount(0);
  await inlineEditor.fill("w".repeat(255));
  const editorLayout = await inlineEditor.evaluate(element => {
    const rect = element.getBoundingClientRect();
    const style = getComputedStyle(element);
    return {
      clientWidth: document.documentElement.clientWidth,
      documentScrollWidth: document.documentElement.scrollWidth,
      inputClientWidth: element.clientWidth,
      inputScrollWidth: element.scrollWidth,
      left: rect.left,
      right: rect.right,
      plainTextOnly: style.backgroundColor === "rgba(0, 0, 0, 0)" &&
        Number.parseFloat(style.borderTopWidth) === 0 &&
        Number.parseFloat(style.borderRightWidth) === 0 &&
        Number.parseFloat(style.borderLeftWidth) === 0 &&
        Number.parseFloat(style.borderBottomWidth) === 0 &&
        style.borderRadius === "0px" &&
        style.outlineStyle === "none" &&
        style.boxShadow === "none",
    };
  });
  expect(editorLayout.documentScrollWidth).toBeLessThanOrEqual(editorLayout.clientWidth);
  expect(editorLayout.left).toBeGreaterThanOrEqual(0);
  expect(editorLayout.right).toBeLessThanOrEqual(editorLayout.clientWidth);
  expect(editorLayout.inputScrollWidth).toBeGreaterThan(editorLayout.inputClientWidth);
  expect(editorLayout.plainTextOnly).toBe(true);
  await inlineEditor.press("Escape");

  const layout = await page.evaluate(() => {
    const actionCell = document.querySelector(".paths-table .cell-actions");
    const rect = actionCell.getBoundingClientRect();
    return {
      clientWidth: document.documentElement.clientWidth,
      scrollWidth: document.documentElement.scrollWidth,
      actionLeft: rect.left,
      actionRight: rect.right,
    };
  });
  expect(layout.clientWidth).toBe(320);
  expect(layout.scrollWidth).toBeLessThanOrEqual(layout.clientWidth);
  expect(layout.actionLeft).toBeGreaterThanOrEqual(0);
  expect(layout.actionRight).toBeLessThanOrEqual(layout.clientWidth);
});

test("强制颜色模式保留行内编辑器焦点与对话框语义", async ({
  appPage: page,
}) => {
  await page.emulateMedia({ forcedColors: "active" });
  expect(
    await page.evaluate(() => matchMedia("(forced-colors: active)").matches),
  ).toBe(true);

  const trigger = page.getByRole("button", { name: "New empty file" });
  await trigger.focus();
  await trigger.click();
  const input = page.locator(".inline-name-input");
  await expect(input).toBeFocused();
  expect(
    await input.evaluate(element => {
      const style = getComputedStyle(element);
      return style.borderTopStyle === "none" &&
        style.borderRightStyle === "none" &&
        style.borderLeftStyle === "none" &&
        style.borderBottomStyle === "none" &&
        style.outlineStyle === "none" &&
        style.boxShadow === "none" &&
        style.caretColor !== "rgba(0, 0, 0, 0)";
    }),
  ).toBe(true);
  await expect(page.locator(".inline-name-marker")).toHaveCount(0);

  await page.keyboard.press("Escape");
  await expect(input).toHaveCount(0);
  await expect(trigger).toBeFocused();
});

test("登录页通过 axe WCAG A/AA 自动扫描", async ({ axePage: page }) => {
  await page.goto("/__dufs__/login");
  const results = await axe(page).analyze();
  expect(results.violations).toEqual([]);
});

test("文件页、行内编辑器和操作对话框通过 axe WCAG A/AA 自动扫描", async ({
  axePage: page,
}, testInfo) => {
  // This case deliberately runs three axe analyses. On the fully-parallel
  // Chromium/Firefox matrix, 30 seconds is not a reliable combined CPU budget.
  test.setTimeout(60_000);
  await login(page, testInfo.parallelIndex);
  const pageResults = await axe(page).analyze();
  expect(pageResults.violations).toEqual([]);

  await page.getByRole("button", { name: "New folder" }).click();
  const input = page.locator(".inline-name-input");
  await expect(input).toBeVisible();
  const editorResults = await axe(page).include(".is-renaming").analyze();
  expect(editorResults.violations).toEqual([]);
  await page.keyboard.press("Escape");

  await rowByName(page, "download-me.txt")
    .getByRole("button", { name: "Delete download-me.txt" })
    .click();
  const dialog = actionDialog(page, "Delete item");
  await expect(dialog).toBeVisible();
  // The page itself was scanned above. Scope the modal-state scan to the open
  // dialog so axe does not rescan the inert page subtree under CPU contention.
  const dialogResults = await axe(page).include(".action-dialog").analyze();
  expect(dialogResults.violations).toEqual([]);
  await page.keyboard.press("Escape");
});

test("生产前端源码不包含动态 HTML 注入接口或浏览器原生模态调用", async () => {
  const modulesDir = resolve(__dirname, "../../assets/modules");
  const files = [
    resolve(__dirname, "../../assets/index.js"),
    resolve(__dirname, "../../assets/login.js"),
    ...walkJavaScript(modulesDir),
  ];
  const forbidden = /\b(?:innerHTML|outerHTML|insertAdjacentHTML|document\.write|DOMParser)\b/;
  const nativeModal = /(?<![.\w])(?:alert|confirm|prompt)\s*\(|\b(?:globalThis|self|window)\s*\.\s*(?:alert|confirm|prompt)\s*\(/u;
  for (const file of files) {
    const source = readFileSync(file, "utf8");
    expect(source, file).not.toMatch(forbidden);
    expect(source, file).not.toMatch(nativeModal);
  }
});

test("生产界面与公开错误源码只包含英文内置文案", async () => {
  const assetsDir = resolve(__dirname, "../../assets");
  const modulesDir = join(assetsDir, "modules");
  const files = [
    join(assetsDir, "index.html"),
    join(assetsDir, "login.html"),
    join(assetsDir, "login.css"),
    join(assetsDir, "index.js"),
    resolve(__dirname, "../../src/server/listing.rs"),
    resolve(__dirname, "../../src/server/session.rs"),
    ...walkJavaScript(modulesDir),
  ];
  for (const file of files) {
    const source = readFileSync(file, "utf8");
    const productionSource = file.endsWith(".rs")
      ? source.split(/#\[cfg\(test\)\]\s*mod tests\s*\{/u, 1)[0]
      : source;
    expect(productionSource, file).not.toMatch(/\p{Script=Han}/u);
  }
});
