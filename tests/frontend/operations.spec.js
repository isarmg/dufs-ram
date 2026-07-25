const { expect, rowByName, test } = require("./fixtures");

test("使用浏览器 API 新建文件夹并进入目录", async ({ appPage: page }) => {
  page.once("dialog", dialog => dialog.accept("created-by-browser"));
  const responsePromise = page.waitForResponse(
    response =>
      response.request().method() === "POST" &&
      new URL(response.url()).pathname.endsWith("/__dufs__/api/mkdir"),
  );
  await page.getByRole("button", { name: "新建文件夹" }).click();
  const response = await responsePromise;
  expect(response.status()).toBe(201);
  expect(response.request().postDataJSON()).toEqual({
    path: "/created-by-browser",
  });
  await page.waitForURL(url => url.pathname.includes("/created-by-browser"));
  await expect(page.locator(".breadcrumb")).toContainText("created-by-browser");
});

test("新建空文件使用严格上传协议", async ({ appPage: page }) => {
  page.once("dialog", dialog => dialog.accept("empty-by-browser.txt"));
  const responsePromise = page.waitForResponse(
    response =>
      response.request().method() === "PUT" &&
      new URL(response.url()).pathname.endsWith("/empty-by-browser.txt"),
  );
  await page.getByRole("button", { name: "新建空文件" }).click();
  const response = await responsePromise;
  expect(response.status()).toBe(201);
  expect(response.request().headers()["x-dufs-upload-length"]).toBe("0");
  expect(response.request().headers()["x-dufs-upload-id"]).toBeTruthy();
  await expect(
    page.getByRole("link", { name: "empty-by-browser.txt", exact: true }),
  ).toBeVisible();
});

test("移动并重命名文件", async ({ appPage: page }) => {
  page.once("dialog", dialog => dialog.accept("/renamed-by-browser.txt"));
  const responsePromise = page.waitForResponse(
    response =>
      response.request().method() === "POST" &&
      new URL(response.url()).pathname.endsWith("/__dufs__/api/move"),
  );
  await rowByName(page, "rename-me.txt")
    .getByRole("button", { name: "移动或重命名 rename-me.txt" })
    .click();
  const response = await responsePromise;
  expect(response.status()).toBe(204);
  expect(response.request().postDataJSON()).toEqual({
    source: "/rename-me.txt",
    destination: "/renamed-by-browser.txt",
    overwrite: false,
  });
  await expect(
    page.getByRole("link", { name: "renamed-by-browser.txt", exact: true }),
  ).toBeVisible();
});

test("目标存在时只有确认后才显式覆盖", async ({
  appPage: page,
  context,
}) => {
  const targetUrl = new URL("/overwrite-target.txt", page.url()).href;
  page.on("dialog", async dialog => {
    if (dialog.type() === "prompt") {
      await dialog.accept("/overwrite-target.txt");
    } else {
      expect(dialog.message()).toContain("确定覆盖");
      await dialog.accept();
    }
  });
  const responses = [];
  page.on("response", response => {
    if (
      response.request().method() === "POST" &&
      new URL(response.url()).pathname.endsWith("/__dufs__/api/move")
    ) {
      responses.push(response);
    }
  });
  await rowByName(page, "overwrite-source.txt")
    .getByRole("button", { name: "移动或重命名 overwrite-source.txt" })
    .click();
  await expect.poll(() => responses.length).toBe(2);
  expect(responses.map(response => response.status())).toEqual([409, 204]);
  expect(responses.map(response => response.request().postDataJSON().overwrite))
    .toEqual([false, true]);
  const contentResponse = await context.request.get(targetUrl);
  expect(contentResponse.status()).toBe(200);
  expect(await contentResponse.text()).toBe("replacement content");
});

test("删除失败时显示错误并保留目录行", async ({ appPage: page }) => {
  await page.route("**/delete-me.txt", async route => {
    if (route.request().method() === "DELETE") {
      await route.fulfill({ status: 500, body: "forced delete failure" });
    } else {
      await route.continue();
    }
  });
  let alertMessage = "";
  page.on("dialog", async dialog => {
    if (dialog.type() === "confirm") {
      await dialog.accept();
    } else {
      alertMessage = dialog.message();
      await dialog.dismiss();
    }
  });
  await rowByName(page, "delete-me.txt")
    .getByRole("button", { name: "删除 delete-me.txt" })
    .click();
  await expect.poll(() => alertMessage).toContain("无法删除“delete-me.txt”");
  await expect(rowByName(page, "delete-me.txt")).toBeVisible();
});

test("删除成功后更新已加载数量并移动焦点", async ({ appPage: page }) => {
  const status = page.locator(".list-status");
  const beforeText = await status.textContent();
  const beforeCount = Number(beforeText.match(/\d+/)?.[0]);
  expect(beforeCount).toBeGreaterThan(0);

  page.once("dialog", dialog => dialog.accept());
  const responsePromise = page.waitForResponse(
    response =>
      response.request().method() === "DELETE" &&
      new URL(response.url()).pathname.endsWith("/delete-me.txt"),
  );
  await rowByName(page, "delete-me.txt")
    .getByRole("button", { name: "删除 delete-me.txt" })
    .click();
  expect((await responsePromise).status()).toBe(204);
  await expect(rowByName(page, "delete-me.txt")).toHaveCount(0);
  await expect(status).toHaveText(`已加载全部 ${beforeCount - 1} 项`);
  expect(
    await page.evaluate(() => document.activeElement !== document.body),
  ).toBe(true);
});

test("非法新建名称显示中文错误且不产生请求", async ({ appPage: page }) => {
  let putRequests = 0;
  let alertMessage = "";
  page.on("request", request => {
    if (request.method() === "PUT") putRequests++;
  });
  page.on("dialog", async dialog => {
    if (dialog.type() === "prompt") {
      await dialog.accept("..");
    } else {
      alertMessage = dialog.message();
      await dialog.dismiss();
    }
  });
  await page.getByRole("button", { name: "新建空文件" }).click();
  await expect.poll(() => alertMessage).toContain(
    "无法创建文件“..”：无效路径",
  );
  expect(putRequests).toBe(0);
});
