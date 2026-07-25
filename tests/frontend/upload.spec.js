const {
  expect,
  pageData,
  rotateSession,
  selectFiles,
  test,
} = require("./fixtures");

test("上传成功后显示状态并且文件内容可读取", async ({ appPage: page }) => {
  const responsePromise = page.waitForResponse(
    response =>
      response.request().method() === "PUT" &&
      new URL(response.url()).pathname.endsWith("/uploaded-smoke.txt"),
  );
  await page.locator("#file").setInputFiles({
    name: "uploaded-smoke.txt",
    mimeType: "text/plain",
    buffer: Buffer.from("uploaded from Playwright"),
  });
  const response = await responsePromise;
  expect(response.status()).toBe(201);
  expect(response.request().headers()["x-dufs-upload-id"]).toMatch(
    /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/,
  );
  await expect(page.locator(".upload-status")).toHaveAttribute(
    "aria-label",
    "uploaded-smoke.txt：上传成功",
  );
  expect(
    await page.evaluate(async () => (await fetch("/uploaded-smoke.txt")).text()),
  ).toBe("uploaded from Playwright");
});

test("网络失败后保留同一页面 File 对象并查询检查点重试", async ({
  appPage: page,
}) => {
  let failFirstPut = true;
  await page.route("**/retry-smoke.txt", async route => {
    if (route.request().method() === "PUT" && failFirstPut) {
      failFirstPut = false;
      await route.abort("connectionfailed");
    } else {
      await route.continue();
    }
  });

  await selectFiles(page, "#file", [{
    name: "retry-smoke.txt",
    mimeType: "text/plain",
    buffer: Buffer.from("retry succeeds"),
    lastModified: 1_722_000_000_301,
  }]);
  const retry = page.getByRole("button", { name: "重试上传 retry-smoke.txt" });
  await expect(retry).toBeVisible();

  const statusPromise = page.waitForResponse(
    response =>
      response.request().method() === "HEAD" &&
      new URL(response.url()).pathname.endsWith("/retry-smoke.txt"),
  );
  const uploadPromise = page.waitForResponse(
    response =>
      response.request().method() === "PUT" &&
      new URL(response.url()).pathname.endsWith("/retry-smoke.txt"),
  );
  await retry.click();
  expect((await statusPromise).status()).toBe(404);
  expect((await uploadPromise).status()).toBe(201);
  await expect(page.locator(".upload-status")).toHaveAttribute(
    "aria-label",
    "retry-smoke.txt：上传成功",
  );
});

test("同页重试读取非零检查点并只 PATCH 剩余内容", async ({
  appPage: page,
}) => {
  let shortenFirstPut = true;
  const methods = [];
  const offsets = [];
  await page.route("**/resume-with-patch.txt", async route => {
    const request = route.request();
    methods.push(request.method());
    offsets.push(request.headers()["x-dufs-upload-offset"] || null);
    if (request.method() === "PUT" && shortenFirstPut) {
      shortenFirstPut = false;
      await route.continue({ postData: Buffer.from("AAAA") });
    } else {
      await route.continue();
    }
  });

  const initialResponse = page.waitForResponse(
    response =>
      response.request().method() === "PUT" &&
      new URL(response.url()).pathname.endsWith("/resume-with-patch.txt"),
  );
  await selectFiles(page, "#file", [{
    name: "resume-with-patch.txt",
    buffer: Buffer.from("AAAABBBB"),
    lastModified: 1_722_000_000_302,
  }]);
  expect((await initialResponse).status()).toBe(409);

  const retry = page.getByRole("button", {
    name: "重试上传 resume-with-patch.txt",
  });
  await expect(retry).toBeVisible();
  await retry.click();
  await expect(page.locator(".upload-status")).toHaveAttribute(
    "aria-label",
    "resume-with-patch.txt：上传成功",
  );
  expect(methods).toEqual(["PUT", "HEAD", "PATCH"]);
  expect(offsets).toEqual([null, null, "4"]);
  expect(
    await page.evaluate(async () =>
      (await fetch("/resume-with-patch.txt")).text()
    ),
  ).toBe("AAAABBBB");
});

test("上传槽占用时重试会排队而不是静默失效", async ({
  appPage: page,
}) => {
  let firstPut = true;
  let releaseSecond;
  let markSecondStarted;
  const secondGate = new Promise(resolve => {
    releaseSecond = resolve;
  });
  const secondStarted = new Promise(resolve => {
    markSecondStarted = resolve;
  });

  await page.route("**/queue-retry-*.txt", async route => {
    const request = route.request();
    const path = new URL(request.url()).pathname;
    if (path.endsWith("/queue-retry-first.txt")) {
      if (request.method() === "PUT" && firstPut) {
        firstPut = false;
        await route.abort("connectionfailed");
      } else if (request.method() === "HEAD") {
        await route.fulfill({ status: 404, body: "" });
      } else {
        await route.fulfill({ status: 201, body: "" });
      }
      return;
    }
    markSecondStarted();
    await secondGate;
    await route.fulfill({ status: 201, body: "" });
  });

  await selectFiles(page, "#file", [
    {
      name: "queue-retry-first.txt",
      buffer: Buffer.from("first"),
      lastModified: 1_722_000_000_303,
    },
    {
      name: "queue-retry-second.txt",
      buffer: Buffer.from("second"),
      lastModified: 1_722_000_000_304,
    },
  ]);
  const retry = page.getByRole("button", {
    name: "重试上传 queue-retry-first.txt",
  });
  await expect(retry).toBeVisible();
  await secondStarted;
  await retry.click();
  await expect(page.locator("#uploadStatus0")).toHaveAttribute(
    "aria-label",
    "queue-retry-first.txt：等待重试",
  );
  releaseSecond();
  await expect(
    page.locator('.upload-status[aria-label$="上传成功"]'),
  ).toHaveCount(2);
});

test("上传进度更新保留取消按钮焦点且取消原因可见", async ({
  appPage: page,
}) => {
  let releaseRequest;
  const requestGate = new Promise(resolve => {
    releaseRequest = resolve;
  });
  await page.route("**/cancel-focus.txt", async route => {
    await requestGate;
    try {
      await route.fulfill({ status: 201, body: "" });
    } catch {
      // The expected browser-side abort may close the intercepted request.
    }
  });
  await page.evaluate(() => {
    const originalSend = XMLHttpRequest.prototype.send;
    XMLHttpRequest.prototype.send = function (...args) {
      window.__dufsTestUploadRequest = this;
      return originalSend.apply(this, args);
    };
  });

  await selectFiles(page, "#file", [{
    name: "cancel-focus.txt",
    buffer: Buffer.from("cancel me"),
    lastModified: 1_722_000_000_305,
  }]);
  const cancel = page.getByRole("button", {
    name: "取消上传 cancel-focus.txt",
  });
  await expect(cancel).toBeVisible();
  await cancel.focus();
  await cancel.evaluate(element => {
    window.__dufsTestCancelButton = element;
  });
  await page.waitForTimeout(350);
  await page.evaluate(() => {
    window.__dufsTestUploadRequest.upload.dispatchEvent(
      new ProgressEvent("progress", {
        lengthComputable: true,
        loaded: 4,
        total: 9,
      }),
    );
  });
  expect(
    await page.evaluate(() =>
      document.activeElement === window.__dufsTestCancelButton &&
      window.__dufsTestCancelButton.isConnected
    ),
  ).toBe(true);
  await cancel.click();
  releaseRequest();
  await expect(page.locator(".upload-failure")).toContainText("上传已取消");
});

test("上传空闲超时会中止请求并显示可见原因", async ({
  appPage: page,
}) => {
  await page.clock.install();
  let releaseRequest;
  const requestGate = new Promise(resolve => {
    releaseRequest = resolve;
  });
  await page.route("**/idle-timeout.txt", async route => {
    await requestGate;
    try {
      await route.fulfill({ status: 201, body: "" });
    } catch {
      // The timeout is expected to close the intercepted request.
    }
  });

  await selectFiles(page, "#file", [{
    name: "idle-timeout.txt",
    buffer: Buffer.from("idle"),
    lastModified: 1_722_000_000_306,
  }]);
  await expect(page.getByRole("button", {
    name: "取消上传 idle-timeout.txt",
  })).toBeVisible();
  await page.clock.fastForward(2 * 60 * 1000 + 1);
  await expect(page.locator(".upload-failure")).toContainText(
    "上传长时间没有进展",
  );
  releaseRequest();
});

test("认证失效会暂停队列并允许直接刷新恢复", async ({ appPage: page }) => {
  const files = [
    {
      name: "queue-auth-a.txt",
      buffer: Buffer.from("queue-a"),
      lastModified: 1_722_000_000_001,
    },
    {
      name: "queue-auth-b.txt",
      buffer: Buffer.from("queue-b"),
      lastModified: 1_722_000_000_002,
    },
  ];
  await rotateSession(page);
  await selectFiles(page, "#file", files);
  await expect(page.locator(".upload-queue-message")).toHaveText(
    "登录状态或当前页面已失效，请刷新页面并重新选择文件。",
  );
  await expect(page.locator(".upload-status")).toHaveCount(2);

  let beforeUnloadDialogs = 0;
  page.on("dialog", async dialog => {
    if (dialog.type() === "beforeunload") beforeUnloadDialogs++;
    await dialog.accept();
  });
  await page.reload();
  await expect(page.locator(".index-page")).toBeVisible();
  expect(beforeUnloadDialogs).toBe(0);

  await selectFiles(page, "#file", files);
  await expect(page.locator('.upload-status[aria-label$="上传成功"]')).toHaveCount(2);
});

test("刷新后不会按同名同大小同 mtime 自动续传另一份内容", async ({
  appPage: page,
}) => {
  const target = "/resume-identity.txt";
  const oldUploadId = "12345678-1234-4123-8123-123456789abc";
  const lastModified = 1_721_234_567_000;
  const data = await pageData(page);
  const checkpoint = await page.evaluate(async payload => {
    const response = await fetch(payload.target, {
      method: "PUT",
      headers: {
        "X-Dufs-CSRF-Token": payload.csrf,
        "X-Dufs-Upload-Id": payload.uploadId,
        "X-Dufs-Upload-Length": "8",
      },
      body: "AAAA",
    });
    return {
      status: response.status,
      offset: response.headers.get("x-dufs-upload-offset"),
    };
  }, {
    target,
    csrf: data.csrf_token,
    uploadId: oldUploadId,
  });
  expect(checkpoint).toEqual({ status: 409, offset: "4" });

  // This legacy-shaped record is injected only to prove that current code
  // ignores it. Production code intentionally has no cross-refresh recovery.
  await page.evaluate(payload => {
    localStorage.setItem("dufs-upload-sessions-v1", JSON.stringify([payload]));
  }, {
    target,
    uploadId: oldUploadId,
    totalLength: 8,
    lastModified,
    fileName: "resume-identity.txt",
    relativePath: "resume-identity.txt",
    user: "frontend-test",
    updatedAt: Date.now(),
  });
  await page.reload();
  await expect(
    page.getByRole("button", { name: "上传文件", exact: true }),
  ).toBeVisible();

  const requests = [];
  page.on("request", request => {
    if (new URL(request.url()).pathname === target) {
      requests.push({
        method: request.method(),
        uploadId: request.headers()["x-dufs-upload-id"],
      });
    }
  });
  await selectFiles(page, "#file", [{
    name: "resume-identity.txt",
    buffer: Buffer.from("BBBB2222"),
    lastModified,
  }]);
  await expect(page.locator(".upload-status")).toHaveAttribute(
    "aria-label",
    "resume-identity.txt：上传成功",
  );
  expect(requests.map(request => request.method)).toEqual(["PUT"]);
  expect(requests[0].uploadId).not.toBe(oldUploadId);
  expect(
    await page.evaluate(async path => (await fetch(path)).text(), target),
  ).toBe("BBBB2222");
});

test("选择超过 128 个文件不会创建或截断浏览器续传记录", async ({
  appPage: page,
}) => {
  test.setTimeout(60_000);
  await page.route("**/bulk-record-*.txt", route => route.fulfill({
    status: 201,
    contentType: "text/plain",
    body: "",
  }));
  await page.evaluate(() => localStorage.clear());
  const files = Array.from({ length: 129 }, (_, index) => ({
    name: `bulk-record-${index}.txt`,
    buffer: Buffer.from(String(index)),
    lastModified: 1_723_000_000_000 + index,
  }));
  await selectFiles(page, "#file", files);
  await expect(page.locator('.upload-status[aria-label$="上传成功"]')).toHaveCount(129);
  expect(
    await page.evaluate(() => localStorage.getItem("dufs-upload-sessions-v1")),
  ).toBeNull();
});

test("拖放被阻止，文件夹选择器保留相对目录", async ({ appPage: page }) => {
  const dropPrevented = await page.evaluate(() => {
    const transfer = new DataTransfer();
    transfer.items.add(new File(["ignored"], "dropped.txt"));
    const event = new Event("drop", { bubbles: true, cancelable: true });
    Object.defineProperty(event, "dataTransfer", { value: transfer });
    document.dispatchEvent(event);
    return event.defaultPrevented;
  });
  expect(dropPrevented).toBe(true);

  await selectFiles(page, "#folder", [{
    name: "nested.txt",
    relativePath: "picked-folder/child/nested.txt",
    buffer: Buffer.from("folder content"),
    lastModified: 1_722_000_000_203,
  }]);
  await expect(page.locator(".upload-status")).toHaveAttribute(
    "aria-label",
    "picked-folder/child/nested.txt：上传成功",
  );
  expect(
    await page.evaluate(async () =>
      (await fetch("/picked-folder/child/nested.txt")).text()
    ),
  ).toBe("folder content");
});
