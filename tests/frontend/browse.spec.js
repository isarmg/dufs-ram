const { currentUrl, expect, login, test } = require("./fixtures");

const dangerousName = `危险 <img src=x onerror=alert(1)> & "'.txt`;
const listingRevision = "a".repeat(64);

test("危险文件名始终作为纯文本节点显示", async ({ appPage: page }) => {
  const link = page.getByRole("link", { name: dangerousName, exact: true });
  await expect(link).toBeVisible();
  expect(await link.textContent()).toBe(dangerousName);
  await expect(page.locator(".paths-table img")).toHaveCount(0);
  expect(
    await page.locator(".paths-table tbody").evaluate(element =>
      element.querySelector("[onerror], script, iframe") !== null
    ),
  ).toBe(false);
});

test("特殊字符搜索保持完整查询词", async ({ appPage: page }) => {
  const query = "special & # + 中文";
  await page.getByLabel("Search files or folders").fill(query);
  await Promise.all([
    page.waitForURL(url => url.searchParams.get("q") === query),
    page.getByLabel("Search files or folders").press("Enter"),
  ]);
  await expect(
    page.getByRole("link", { name: "special & # + 中文.txt", exact: true }),
  ).toBeVisible();
});

test("排序链接只传播支持的参数并暴露排序状态", async ({ appPage: page }) => {
  const target = new URL(page.url());
  target.search = "?q=existing&unused=1";
  await page.goto(target.href);
  const nameHeader = page.locator(".paths-table thead th").first();
  const link = nameHeader.getByRole("link");
  const url = new URL(await link.getAttribute("href"), page.url());
  expect([...url.searchParams.keys()].sort()).toEqual(["order", "q", "sort"]);
  expect(url.searchParams.get("q")).toBe("existing");
  expect(url.searchParams.get("sort")).toBe("name");
  expect(url.searchParams.get("order")).toBe("desc");
  await expect(nameHeader).toHaveAttribute("aria-sort", "ascending");
});

test("目录 API 每页最多加载 200 项并可继续追加", async ({ page }, testInfo) => {
  let calls = 0;
  await page.route("**/__dufs__/api/list?**", async route => {
    calls++;
    const url = new URL(route.request().url());
    expect(url.searchParams.get("path")).toBe("/");
    expect(url.searchParams.get("limit")).toBe("200");
    const cursor = url.searchParams.get("cursor");
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify(cursor === null
        ? {
          paths: [
            { path_type: "Dir", name: "existing-folder", mtime: 0, size: 0, revision: listingRevision },
            { path_type: "File", name: "page-one.txt", mtime: 0, size: 1, revision: listingRevision },
          ],
          next_cursor: "opaque-next",
        }
        : {
          paths: [
            { path_type: "File", name: "page-two.txt", mtime: 0, size: 2, revision: listingRevision },
          ],
          next_cursor: null,
        }),
    });
  });

  await login(page, testInfo.parallelIndex);
  await expect(page.getByRole("link", {
    name: "Download folder existing-folder",
  })).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Load more" })).toBeVisible();
  await page.getByRole("button", { name: "Load more" }).click();
  await expect(
    page.getByRole("link", { name: "page-two.txt", exact: true }),
  ).toBeVisible();
  await expect(page.getByRole("button", { name: "Load more" })).toBeHidden();
  await expect(page.locator(".list-status")).toHaveText("All 3 items loaded");
  await expect(page.locator(".list-status")).toBeFocused();
  expect(calls).toBe(2);
});

test("目录请求超时后中止并提供可重试状态", async ({ appPage: page }) => {
  let calls = 0;
  let releaseRequest;
  const requestGate = new Promise(resolve => {
    releaseRequest = resolve;
  });
  await page.route("**/__dufs__/api/list?**", async route => {
    calls++;
    if (calls === 1) {
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({
          paths: [{
            path_type: "File",
            name: "first-page.txt",
            mtime: 0,
            size: 1,
            revision: listingRevision,
          }],
          next_cursor: "timeout-page",
        }),
      });
      return;
    }
    await requestGate;
    try {
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({ paths: [], next_cursor: null }),
      });
    } catch {
      // The client-side deadline is expected to close the intercepted request.
    }
  });

  await page.reload();
  const loadMore = page.getByRole("button", { name: "Load more" });
  await expect(loadMore).toBeVisible();
  await page.clock.install();
  await loadMore.click();
  await page.clock.fastForward(30 * 1000 + 1);
  await expect(page.locator(".list-status")).toContainText(
    "Unable to load the file list: Request timed out. Try again.",
  );
  await expect(page.getByRole("button", { name: "Retry" })).toBeEnabled();
  releaseRequest();
});

test("不存在的目录直接显示可上传空态且不请求列表", async ({
  appPage: page,
}) => {
  let listRequests = 0;
  page.on("request", request => {
    if (new URL(request.url()).pathname.endsWith("/__dufs__/api/list")) {
      listRequests++;
    }
  });
  await page.goto(currentUrl(page, "not-created-yet/"));
  await expect(page.locator(".empty-folder")).toHaveText(
    "Uploading files will create this folder automatically",
  );
  await expect(page.locator(".list-status")).toBeEmpty();
  expect(listRequests).toBe(0);
});

test("搜索支持 128 个多字节字符", async ({ appPage: page }) => {
  const query = "文".repeat(128);
  const responsePromise = page.waitForResponse(response => {
    const url = new URL(response.url());
    return url.pathname.endsWith("/__dufs__/api/list") &&
      url.searchParams.get("q") === query;
  });
  await page.getByLabel("Search files or folders").fill(query);
  await page.getByLabel("Search files or folders").press("Enter");
  expect((await responsePromise).status()).toBe(200);
  await expect(page.locator(".empty-folder")).toHaveText("No search results");
  await expect(page.locator(".list-status")).not.toContainText("Unable to load");
});

test("文件链接下载附件且 Range 下载保持可用", async ({ appPage: page }) => {
  const target = currentUrl(page, "download-me.txt");
  const fileLink = page.getByRole("link", {
    name: "download-me.txt",
    exact: true,
  });
  const downloadPromise = page.waitForEvent("download");
  await fileLink.click();
  const download = await downloadPromise;
  expect(download.suggestedFilename()).toBe("download-me.txt");
  const stream = await download.createReadStream();
  const chunks = [];
  for await (const chunk of stream) chunks.push(chunk);
  expect(Buffer.concat(chunks).toString()).toBe("downloaded by browser test");

  expect(
    await page.evaluate(async url => {
      const response = await fetch(url, {
        headers: { Range: "bytes=0-4" },
      });
      return {
        status: response.status,
        range: response.headers.get("content-range"),
        body: await response.text(),
      };
    }, target),
  ).toEqual({
    status: 206,
    range: "bytes 0-4/26",
    body: "downl",
  });
});

test("目录页在整页校验失败时不提交部分 DOM", async ({ appPage: page }) => {
  await page.route("**/__dufs__/api/list?**", route => route.fulfill({
    status: 200,
    contentType: "application/json",
    body: JSON.stringify({
      paths: [
        { path_type: "File", name: "valid-before-error.txt", mtime: 0, size: 1, revision: listingRevision },
        { path_type: "File", name: "../invalid.txt", mtime: 0, size: 1, revision: listingRevision },
      ],
      next_cursor: null,
    }),
  }));
  await page.reload();
  await expect(page.locator(".list-status")).toContainText(
    "Unable to load the file list: Invalid file list item",
  );
  await expect(page.getByRole("link", {
    name: "valid-before-error.txt",
  })).toHaveCount(0);
  await expect(page.locator(".paths-table tbody tr")).toHaveCount(0);
});

test("目录页拒绝重复游标且保留上一页", async ({ appPage: page }) => {
  let calls = 0;
  await page.route("**/__dufs__/api/list?**", route => {
    calls++;
    return route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({
        paths: [{
          path_type: "File",
          name: calls === 1 ? "cursor-page-one.txt" : "cursor-page-two.txt",
          mtime: 0,
          size: 1,
          revision: listingRevision,
        }],
        next_cursor: "repeated-cursor",
      }),
    });
  });
  await page.reload();
  await page.getByRole("button", { name: "Load more" }).click();
  await expect(page.locator(".list-status")).toContainText(
    "The server repeated a file list cursor",
  );
  await expect(page.getByRole("link", {
    name: "cursor-page-one.txt",
    exact: true,
  })).toBeVisible();
  await expect(page.getByRole("link", {
    name: "cursor-page-two.txt",
    exact: true,
  })).toHaveCount(0);
});

test("大量目录项使用可访问窗口限制 DOM 数量", async ({ appPage: page }) => {
  test.slow();
  let pageIndex = 0;
  await page.route("**/__dufs__/api/list?**", route => {
    const current = pageIndex++;
    const paths = Array.from({ length: 200 }, (_, offset) => ({
      path_type: "File",
      name: `window-${current * 200 + offset}.txt`,
      mtime: 0,
      size: offset,
      revision: listingRevision,
    }));
    return route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({
        paths,
        next_cursor: current < 1 ? `cursor-${current + 1}` : null,
      }),
    });
  });
  await page.reload();
  for (let index = 0; index < 1; index++) {
    await page.getByRole("button", { name: "Load more" }).click();
    await expect(page.locator(".list-status")).toContainText(
      `${(index + 2) * 200} items loaded`,
    );
  }
  await expect(page.locator(".paths-table tbody tr")).toHaveCount(200);
  const previous = page.getByRole("button", { name: "Show previous items" });
  await expect(previous).toBeVisible();
  await previous.click();
  await expect(page.getByRole("link", {
    name: "window-0.txt",
    exact: true,
  })).toBeVisible();
  await expect(page.getByRole("button", { name: "Show next items" })).toBeVisible();

  await page.getByRole("button", { name: "New empty file" }).click();
  const inlineEditor = page.locator(".inline-name-input");
  await expect(inlineEditor).toHaveValue("newfile");
  await expect(inlineEditor).toBeFocused();
  await expect(page.locator(".paths-table tbody tr")).toHaveCount(200);
  await expect(page.locator(".paths-table tbody tr.is-renaming")).toHaveCount(1);
  const shiftedFirstRow = page.locator("#addPath1");
  await expect(shiftedFirstRow.getByRole("link", {
    name: "window-0.txt",
    exact: true,
  })).toBeVisible();
  for (const action of ["move", "delete", "rename"]) {
    await expect(shiftedFirstRow.locator(
      `button[data-action="${action}"]`,
    )).toHaveAttribute("data-index", "1");
  }
  await expect(page.getByRole("link", {
    name: "window-199.txt",
    exact: true,
  })).toHaveCount(0);
});
