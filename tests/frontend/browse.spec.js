const { expect, login, test } = require("./fixtures");

const dangerousName = `危险 <img src=x onerror=alert(1)> & "'.txt`;

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
  await page.getByLabel("搜索文件或文件夹").fill(query);
  await Promise.all([
    page.waitForURL(url => url.searchParams.get("q") === query),
    page.getByLabel("搜索文件或文件夹").press("Enter"),
  ]);
  await expect(
    page.getByRole("link", { name: "special & # + 中文.txt", exact: true }),
  ).toBeVisible();
});

test("排序链接只传播支持的参数并暴露排序状态", async ({ appPage: page }) => {
  await page.goto("/?q=existing&unused=1");
  const nameHeader = page.locator(".paths-table thead th").first();
  const link = nameHeader.getByRole("link");
  const url = new URL(await link.getAttribute("href"), page.url());
  expect([...url.searchParams.keys()].sort()).toEqual(["order", "q", "sort"]);
  expect(url.searchParams.get("q")).toBe("existing");
  expect(url.searchParams.get("sort")).toBe("name");
  expect(url.searchParams.get("order")).toBe("desc");
  await expect(nameHeader).toHaveAttribute("aria-sort", "ascending");
});

test("目录 API 每页最多加载 200 项并可继续追加", async ({ page }) => {
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
            { path_type: "Dir", name: "existing-folder", mtime: 0, size: 0 },
            { path_type: "File", name: "page-one.txt", mtime: 0, size: 1 },
          ],
          next_cursor: "opaque-next",
        }
        : {
          paths: [
            { path_type: "File", name: "page-two.txt", mtime: 0, size: 2 },
          ],
          next_cursor: null,
        }),
    });
  });

  await login(page);
  await expect(page.getByRole("button", { name: "加载更多" })).toBeVisible();
  await page.getByRole("button", { name: "加载更多" }).click();
  await expect(
    page.getByRole("link", { name: "page-two.txt", exact: true }),
  ).toBeVisible();
  await expect(page.getByRole("button", { name: "加载更多" })).toBeHidden();
  await expect(page.locator(".list-status")).toHaveText("已加载全部 3 项");
  await expect(page.locator(".list-status")).toBeFocused();
  expect(calls).toBe(2);
});

test("不存在的目录直接显示可上传空态且不请求列表或 ZIP", async ({
  appPage: page,
}) => {
  let listRequests = 0;
  page.on("request", request => {
    if (new URL(request.url()).pathname.endsWith("/__dufs__/api/list")) {
      listRequests++;
    }
  });
  await page.goto("/not-created-yet/");
  await expect(page.locator(".empty-folder")).toHaveText(
    "上传文件时将自动创建此文件夹",
  );
  await expect(page.locator(".download")).toBeHidden();
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
  await page.getByLabel("搜索文件或文件夹").fill(query);
  await page.getByLabel("搜索文件或文件夹").press("Enter");
  expect((await responsePromise).status()).toBe(200);
  await expect(page.locator(".empty-folder")).toHaveText("没有搜索结果");
  await expect(page.locator(".list-status")).not.toContainText("无法加载");
});

test("文件链接下载附件且 Range 下载保持可用", async ({ appPage: page }) => {
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
    await page.evaluate(async () => {
      const response = await fetch("/download-me.txt", {
        headers: { Range: "bytes=0-4" },
      });
      return {
        status: response.status,
        range: response.headers.get("content-range"),
        body: await response.text(),
      };
    }),
  ).toEqual({
    status: 206,
    range: "bytes 0-4/26",
    body: "downl",
  });
});
