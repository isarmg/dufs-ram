const { randomUUID } = require("node:crypto");
const {
  currentLogicalChild,
  currentUrl,
  expect,
  pageData,
  rotateSession,
  selectFiles,
  test,
} = require("./fixtures");

function protocolHeaders(request, state, offset = null, storedLength = null) {
  const headers = request.headers();
  const length = storedLength ?? headers["x-dufs-upload-length"];
  return {
    "X-Dufs-Upload-Id": headers["x-dufs-upload-id"],
    "X-Dufs-Operation-State": state,
    ...(length === undefined ? {} : { "X-Dufs-Upload-Length": length }),
    ...(offset === null
      ? {}
      : { "X-Dufs-Upload-Offset": offset === "length" ? length : String(offset) }),
  };
}

function discardProtocolHeaders(uploadId, length, offset = length) {
  return {
    "X-Dufs-Upload-Id": uploadId,
    "X-Dufs-Upload-Length": String(length),
    "X-Dufs-Upload-Offset": String(offset),
    "X-Dufs-Operation-State": "rejected",
  };
}

function problemDetails(status, code, detail, recovery = "", extensions = {}) {
  return JSON.stringify({
    type: `urn:dufs:problem:${code}`,
    title: "Upload failed",
    status,
    detail,
    code,
    ...(recovery ? { recovery } : {}),
    ...extensions,
  });
}

test("无冲突选择不弹确认并直接上传", async ({ appPage: page }) => {
  const target = currentUrl(page, "uploaded-smoke.txt");
  const responsePromise = page.waitForResponse(
    response =>
      response.request().method() === "PUT" &&
      new URL(response.url()).pathname.endsWith("/uploaded-smoke.txt"),
  );
  await selectFiles(page, "#file", [{
    name: "uploaded-smoke.txt",
    mimeType: "text/plain",
    buffer: Buffer.from("uploaded from Playwright"),
  }]);
  await expect(page.getByRole("dialog", {
    name: "Existing upload destinations",
  })).toBeHidden();
  const response = await responsePromise;
  expect(response.status()).toBe(201);
  expect(response.request().headers()["x-dufs-upload-id"]).toMatch(
    /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/,
  );
  await expect(page.locator(".upload-status")).toHaveAttribute(
    "aria-label",
    "uploaded-smoke.txt: upload complete",
  );
  expect(
    await page.evaluate(async url => (await fetch(url)).text(), target),
  ).toBe("uploaded from Playwright");
  await page.getByRole("button", { name: "Refresh", exact: true }).click();
  await expect(page.getByRole("table", { name: "File list" }).getByRole(
    "link",
    { name: "uploaded-smoke.txt", exact: true },
  )).toBeVisible();
});

test("预检发现的批量冲突只确认一次且确认前不发送 PUT", async ({
  appPage: page,
}) => {
  const targetNames = ["download-me.txt", "overwrite-target.txt"];
  let putCount = 0;
  page.on("request", request => {
    if (
      request.method() === "PUT" &&
      targetNames.some(name => new URL(request.url()).pathname.endsWith(`/${name}`))
    ) {
      putCount++;
    }
  });

  await selectFiles(page, "#file", targetNames.map((name, index) => ({
    name,
    mimeType: "text/plain",
    buffer: Buffer.from(`replacement-${index}`),
  })));
  const dialog = page.getByRole("dialog", {
    name: "Existing upload destinations",
    exact: true,
  });
  await expect(dialog).toContainText("download-me.txt");
  await expect(dialog).toContainText("overwrite-target.txt");
  await expect(dialog).toContainText("already exist");
  expect(putCount).toBe(0);

  await dialog.getByRole("button", { name: "Overwrite", exact: true }).click();
  await expect(
    page.locator('.upload-status[aria-label$="upload complete"]'),
  ).toHaveCount(2);
  expect(putCount).toBe(2);
  await expect(dialog).toBeHidden();
});

test("目录选择中的嵌套目标即使不在当前 DOM 也由预检识别", async ({
  appPage: page,
}) => {
  const target = currentUrl(page, "existing-folder/nested.txt");
  let putCount = 0;
  page.on("request", request => {
    if (
      request.method() === "PUT" &&
      new URL(request.url()).pathname.endsWith("/existing-folder/nested.txt")
    ) {
      putCount++;
    }
  });

  await selectFiles(page, "#folder", [{
    name: "nested.txt",
    relativePath: "existing-folder/nested.txt",
    mimeType: "text/plain",
    buffer: Buffer.from("nested replacement"),
  }]);
  const dialog = page.getByRole("dialog", {
    name: "Existing upload destinations",
    exact: true,
  });
  await expect(dialog).toContainText("existing-folder/nested.txt");
  expect(putCount).toBe(0);

  await dialog.getByRole("button", { name: "Overwrite", exact: true }).click();
  await expect(page.locator(".upload-status")).toHaveAttribute(
    "aria-label",
    "existing-folder/nested.txt: upload complete",
  );
  expect(putCount).toBe(1);
  expect(
    await page.evaluate(async url => (await fetch(url)).text(), target),
  ).toBe("nested replacement");
});

test("跳过预检冲突仍上传同批次中的新文件", async ({ appPage: page }) => {
  const existingUrl = currentUrl(page, "download-me.txt");
  const newUrl = currentUrl(page, "preflight-skip-new.txt");
  const putTargets = [];
  page.on("request", request => {
    if (request.method() === "PUT") {
      putTargets.push(new URL(request.url()).pathname);
    }
  });

  await selectFiles(page, "#file", [
    {
      name: "download-me.txt",
      buffer: Buffer.from("must be skipped"),
    },
    {
      name: "preflight-skip-new.txt",
      buffer: Buffer.from("new file survives"),
    },
  ]);
  const dialog = page.getByRole("dialog", {
    name: "Existing upload destinations",
    exact: true,
  });
  await dialog.getByRole("button", {
    name: "Skip conflicts",
    exact: true,
  }).click();

  await expect(page.locator(".upload-status")).toHaveAttribute(
    "aria-label",
    "preflight-skip-new.txt: upload complete",
  );
  expect(putTargets.some(path => path.endsWith("/download-me.txt"))).toBe(false);
  expect(putTargets.some(path => path.endsWith("/preflight-skip-new.txt"))).toBe(true);
  expect(
    await page.evaluate(async url => (await fetch(url)).text(), existingUrl),
  ).toBe("downloaded by browser test");
  expect(
    await page.evaluate(async url => (await fetch(url)).text(), newUrl),
  ).toBe("new file survives");
});

test("预检后且 PUT 前目标变化时再次确认并只采用服务器新 revision", async ({
  appPage: page,
}) => {
  const changedRevision = "b".repeat(64);
  const requests = [];
  await page.route("**/download-me.txt", async route => {
    const request = route.request();
    if (request.method() !== "PUT") {
      await route.continue();
      return;
    }
    requests.push({
      uploadId: request.headers()["x-dufs-upload-id"],
      overwrite: request.headers()["x-dufs-upload-overwrite"],
      revision: request.headers()["x-dufs-target-revision"],
      body: request.postDataBuffer()?.toString() || "",
    });
    if (requests.length === 1) {
      await route.fulfill({
        status: 409,
        contentType: "application/problem+json",
        headers: {
          ...protocolHeaders(request, "not-started"),
          "X-Dufs-Target-Revision": changedRevision,
          "X-Dufs-Target-Replaceable": "true",
        },
        body: problemDetails(
          409,
          "destination_exists",
          "Destination changed after preflight",
          "refresh_target",
        ),
      });
      return;
    }
    await route.fulfill({
      status: 201,
      headers: protocolHeaders(request, "committed", "length"),
      body: "",
    });
  });

  await selectFiles(page, "#file", [{
    name: "download-me.txt",
    buffer: Buffer.from("confirmed replacement"),
  }]);
  const initial = page.getByRole("dialog", {
    name: "Existing upload destinations",
    exact: true,
  });
  await initial.getByRole("button", { name: "Overwrite", exact: true }).click();

  const changed = page.getByRole("dialog", {
    name: "Upload destination changed",
    exact: true,
  });
  await expect(changed).toContainText("before its data was sent");
  expect(requests).toHaveLength(1);
  expect(requests[0].overwrite).toBe("true");
  expect(requests[0].revision).toMatch(/^[0-9a-f]{64}$/);
  expect(requests[0].revision).not.toBe(changedRevision);
  await changed.getByRole("button", { name: "Overwrite", exact: true }).click();

  await expect(page.locator(".upload-status")).toHaveAttribute(
    "aria-label",
    "download-me.txt: upload complete",
  );
  expect(requests).toHaveLength(2);
  expect(requests[1]).toMatchObject({
    uploadId: requests[0].uploadId,
    overwrite: "true",
    revision: changedRevision,
    body: "confirmed replacement",
  });
});

test("可信普通冲突选择 Skip 时使列表失效且不覆盖", async ({
  appPage: page,
}) => {
  const changedRevision = "8".repeat(64);
  const target = currentUrl(page, "download-me.txt");
  const putRequests = [];
  await page.route("**/download-me.txt", async route => {
    const request = route.request();
    if (request.method() !== "PUT") {
      await route.continue();
      return;
    }
    putRequests.push({
      overwrite: request.headers()["x-dufs-upload-overwrite"],
      revision: request.headers()["x-dufs-target-revision"] || null,
    });
    await route.fulfill({
      status: 409,
      contentType: "application/problem+json",
      headers: {
        ...protocolHeaders(request, "not-started"),
        "X-Dufs-Target-Revision": changedRevision,
        "X-Dufs-Target-Replaceable": "true",
      },
      body: problemDetails(
        409,
        "destination_exists",
        "Destination changed after preflight",
        "refresh_target",
      ),
    });
  });

  await selectFiles(page, "#file", [{
    name: "download-me.txt",
    buffer: Buffer.from("must not replace the destination"),
  }]);
  const initial = page.getByRole("dialog", {
    name: "Existing upload destinations",
    exact: true,
  });
  await initial.getByRole("button", { name: "Overwrite", exact: true }).click();

  const changed = page.getByRole("dialog", {
    name: "Upload destination changed",
    exact: true,
  });
  await expect(changed).toBeVisible();
  await changed.getByRole("button", { name: "Skip file", exact: true }).click();

  await expect(page.locator(".upload-status")).toHaveAttribute(
    "aria-label",
    "download-me.txt: skipped because the destination exists",
  );
  await expect(page.getByRole("button", {
    name: "Refresh",
    exact: true,
  })).toBeVisible();
  await expect(page.locator(".list-status")).toContainText(
    "The folder snapshot is stale",
  );
  expect(putRequests).toHaveLength(1);
  expect(putRequests[0].overwrite).toBe("true");
  expect(putRequests[0].revision).toMatch(/^[0-9a-f]{64}$/);
  expect(putRequests[0].revision).not.toBe(changedRevision);
  expect(
    await page.evaluate(async url => (await fetch(url)).text(), target),
  ).toBe("downloaded by browser test");
});

test("提交时的重名确认用空 PATCH 发布已上传的暂存内容", async ({
  appPage: page,
}) => {
  const revision = "c".repeat(64);
  const requests = [];
  await page.route("**/late-stage-conflict.txt", async route => {
    const request = route.request();
    requests.push({
      method: request.method(),
      uploadId: request.headers()["x-dufs-upload-id"],
      overwrite: request.headers()["x-dufs-upload-overwrite"],
      revision: request.headers()["x-dufs-target-revision"] || null,
      length: request.headers()["x-dufs-upload-length"],
      offset: request.headers()["x-dufs-upload-offset"] || null,
      bodyLength: request.postDataBuffer()?.length || 0,
    });
    if (request.method() === "PUT") {
      await route.fulfill({
        status: 409,
        contentType: "application/problem+json",
        headers: {
          ...protocolHeaders(request, "awaiting-confirmation", "length"),
          "X-Dufs-Target-Revision": revision,
          "X-Dufs-Target-Replaceable": "true",
        },
        body: problemDetails(
          409,
          "destination_exists",
          "Destination appeared while uploading",
          "refresh_target",
        ),
      });
      return;
    }
    await route.fulfill({
      status: 204,
      headers: protocolHeaders(request, "committed", "length"),
      body: "",
    });
  });

  const contents = Buffer.from("already uploaded once");
  await selectFiles(page, "#file", [{
    name: "late-stage-conflict.txt",
    buffer: contents,
  }]);
  const dialog = page.getByRole("dialog", {
    name: "Upload destination changed",
    exact: true,
  });
  await expect(dialog).toContainText("uploaded data is staged");
  await dialog.getByRole("button", { name: "Overwrite", exact: true }).click();

  await expect(page.locator(".upload-status")).toHaveAttribute(
    "aria-label",
    "late-stage-conflict.txt: upload complete",
  );
  expect(requests).toHaveLength(2);
  expect(requests[0]).toMatchObject({
    method: "PUT",
    length: String(contents.length),
    offset: null,
    overwrite: "false",
    revision: null,
    bodyLength: contents.length,
  });
  expect(requests[1]).toEqual({
    method: "PATCH",
    uploadId: requests[0].uploadId,
    length: String(contents.length),
    offset: String(contents.length),
    overwrite: "true",
    revision,
    bodyLength: 0,
  });
});

test("可信暂存冲突丢弃后 Skip 时使列表失效且不覆盖", async ({
  appPage: page,
}) => {
  const revision = "9".repeat(64);
  const contents = Buffer.from("staged data must be discarded");
  const requests = [];
  const discards = [];
  let logicalPath = "";
  await page.route("**/__dufs__/api/upload/preflight", async route => {
    const { paths } = JSON.parse(route.request().postData() || "{}");
    expect(paths).toHaveLength(1);
    [logicalPath] = paths;
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({
        targets: [{
          path: logicalPath,
          exists: false,
          revision: null,
          replaceable: true,
        }],
      }),
    });
  });
  await page.route("**/__dufs__/api/upload/discard", async route => {
    const payload = JSON.parse(route.request().postData() || "{}");
    discards.push(payload);
    await route.fulfill({
      status: 204,
      headers: discardProtocolHeaders(payload.upload_id, contents.length),
      body: "",
    });
  });
  await page.route("**/staged-skip-target.txt", async route => {
    const request = route.request();
    requests.push({
      method: request.method(),
      uploadId: request.headers()["x-dufs-upload-id"],
      overwrite: request.headers()["x-dufs-upload-overwrite"],
      revision: request.headers()["x-dufs-target-revision"] || null,
    });
    await route.fulfill({
      status: 409,
      contentType: "application/problem+json",
      headers: {
        ...protocolHeaders(request, "awaiting-confirmation", "length"),
        "X-Dufs-Target-Revision": revision,
        "X-Dufs-Target-Replaceable": "true",
      },
      body: problemDetails(
        409,
        "destination_exists",
        "Destination appeared while uploading",
        "refresh_target",
      ),
    });
  });

  await selectFiles(page, "#file", [{
    name: "staged-skip-target.txt",
    buffer: contents,
  }]);
  const changed = page.getByRole("dialog", {
    name: "Upload destination changed",
    exact: true,
  });
  await expect(changed).toContainText("uploaded data is staged");
  await changed.getByRole("button", { name: "Skip file", exact: true }).click();

  await expect(page.locator(".upload-status")).toHaveAttribute(
    "aria-label",
    "staged-skip-target.txt: skipped because the destination exists",
  );
  await expect(page.getByRole("button", {
    name: "Refresh",
    exact: true,
  })).toBeVisible();
  await expect(page.locator(".list-status")).toContainText(
    "The folder snapshot is stale",
  );
  expect(requests).toHaveLength(1);
  expect(requests[0]).toMatchObject({
    method: "PUT",
    overwrite: "false",
    revision: null,
  });
  expect(discards).toEqual([{
    path: logicalPath,
    upload_id: requests[0].uploadId,
  }]);
});

test("缺少可信 revision 的冲突响应进入 unknown 且绝不弹覆盖确认", async ({
  appPage: page,
}) => {
  const methods = [];
  await page.route("**/untrusted-late-conflict.txt", async route => {
    const request = route.request();
    methods.push(request.method());
    await route.fulfill({
      status: 409,
      contentType: "application/problem+json",
      headers: {
        ...protocolHeaders(request, "awaiting-confirmation", "length"),
        "X-Dufs-Target-Replaceable": "true",
      },
      body: problemDetails(
        409,
        "destination_exists",
        "Malformed conflict authority",
        "refresh_target",
      ),
    });
  });

  await selectFiles(page, "#file", [{
    name: "untrusted-late-conflict.txt",
    buffer: Buffer.from("must never be auto-overwritten"),
  }]);
  await expect(page.locator(".upload-unknown")).toContainText(
    /inconsistent upload response/i,
  );
  await expect(page.locator(".upload-queue-message")).toContainText(
    "remaining upload queue is paused",
  );
  await expect(page.getByRole("dialog", {
    name: "Upload destination changed",
  })).toBeHidden();
  expect(methods).toEqual(["PUT"]);
});

test("提交前目标确认已消失时用 overwrite=false 重新发送完整 PUT", async ({
  appPage: page,
}) => {
  const initialRevision = "d".repeat(64);
  const requests = [];
  await page.route("**/__dufs__/api/upload/preflight", async route => {
    const { paths } = JSON.parse(route.request().postData() || "{}");
    expect(paths).toHaveLength(1);
    expect(paths[0]).toMatch(/\/target-gone-before-put\.txt$/);
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({
        targets: [{
          path: paths[0],
          exists: true,
          revision: initialRevision,
          replaceable: true,
        }],
      }),
    });
  });
  await page.route("**/target-gone-before-put.txt", async route => {
    const request = route.request();
    requests.push({
      method: request.method(),
      uploadId: request.headers()["x-dufs-upload-id"],
      overwrite: request.headers()["x-dufs-upload-overwrite"],
      revision: request.headers()["x-dufs-target-revision"] || null,
      body: request.postDataBuffer()?.toString() || "",
    });
    if (requests.length === 1) {
      await route.fulfill({
        status: 409,
        contentType: "application/problem+json",
        headers: {
          ...protocolHeaders(request, "not-started"),
          "X-Dufs-Target-Replaceable": "true",
        },
        body: problemDetails(
          409,
          "upload_target_changed",
          "The previously observed target is now missing",
          "refresh_target",
        ),
      });
      return;
    }
    await route.fulfill({
      status: 201,
      headers: protocolHeaders(request, "committed", "length"),
      body: "",
    });
  });

  await selectFiles(page, "#file", [{
    name: "target-gone-before-put.txt",
    buffer: Buffer.from("safe create retry"),
  }]);
  const initial = page.getByRole("dialog", {
    name: "Existing upload destinations",
    exact: true,
  });
  await initial.getByRole("button", { name: "Overwrite", exact: true }).click();
  await expect(page.locator(".upload-status")).toHaveAttribute(
    "aria-label",
    "target-gone-before-put.txt: upload complete",
  );
  await expect(page.getByRole("dialog", {
    name: "Upload destination changed",
  })).toBeHidden();
  expect(requests).toEqual([
    {
      method: "PUT",
      uploadId: requests[0].uploadId,
      overwrite: "true",
      revision: initialRevision,
      body: "safe create retry",
    },
    {
      method: "PUT",
      uploadId: requests[0].uploadId,
      overwrite: "false",
      revision: null,
      body: "safe create retry",
    },
  ]);
});

test("服务器拒绝复用旧元数据 stage 时丢弃并以新 ID 完整 PUT", async ({
  appPage: page,
}) => {
  const initialRevision = "e".repeat(64);
  const requests = [];
  const discards = [];
  let logicalPath = "";
  const contents = Buffer.from("stage can become a create");
  let releaseDiscard;
  let markDiscardStarted;
  const discardGate = new Promise(resolve => {
    releaseDiscard = resolve;
  });
  const discardStarted = new Promise(resolve => {
    markDiscardStarted = resolve;
  });
  await page.route("**/__dufs__/api/upload/preflight", async route => {
    const { paths } = JSON.parse(route.request().postData() || "{}");
    expect(paths).toHaveLength(1);
    expect(paths[0]).toMatch(/\/target-gone-after-stage\.txt$/);
    [logicalPath] = paths;
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({
        targets: [{
          path: paths[0],
          exists: true,
          revision: initialRevision,
          replaceable: true,
        }],
      }),
    });
  });
  await page.route("**/__dufs__/api/upload/discard", async route => {
    const payload = JSON.parse(route.request().postData() || "{}");
    discards.push(payload);
    markDiscardStarted();
    await discardGate;
    await route.fulfill({
      status: 204,
      headers: discardProtocolHeaders(payload.upload_id, contents.length),
      body: "",
    });
  });
  await page.route("**/target-gone-after-stage.txt", async route => {
    const request = route.request();
    requests.push({
      method: request.method(),
      uploadId: request.headers()["x-dufs-upload-id"],
      length: request.headers()["x-dufs-upload-length"],
      offset: request.headers()["x-dufs-upload-offset"] || null,
      overwrite: request.headers()["x-dufs-upload-overwrite"],
      revision: request.headers()["x-dufs-target-revision"] || null,
      bodyLength: request.postDataBuffer()?.length || 0,
    });
    if (requests.length === 1) {
      await route.fulfill({
        status: 409,
        contentType: "application/problem+json",
        headers: protocolHeaders(request, "awaiting-confirmation", "length"),
        body: problemDetails(
          409,
          "upload_metadata_preservation_refused",
          "The retained stage contains metadata from a replaced target",
          "refresh_target",
        ),
      });
      return;
    }
    await route.fulfill({
      status: 201,
      headers: protocolHeaders(request, "committed", "length"),
      body: "",
    });
  });

  await selectFiles(page, "#file", [{
    name: "target-gone-after-stage.txt",
    buffer: contents,
  }]);
  const initial = page.getByRole("dialog", {
    name: "Existing upload destinations",
    exact: true,
  });
  await initial.getByRole("button", { name: "Overwrite", exact: true }).click();
  await discardStarted;
  const cleanupStatus = page.locator("#uploadStatus0");
  await expect(cleanupStatus).toHaveAttribute(
    "aria-label",
    "target-gone-after-stage.txt: cleaning up staged upload",
  );
  await expect(cleanupStatus.getByRole("button")).toHaveCount(0);
  releaseDiscard();
  await expect(page.locator(".upload-status")).toHaveAttribute(
    "aria-label",
    "target-gone-after-stage.txt: upload complete",
  );
  await expect(page.getByRole("dialog", {
    name: "Upload destination changed",
  })).toBeHidden();
  expect(requests).toEqual([
    {
      method: "PUT",
      uploadId: requests[0].uploadId,
      length: String(contents.length),
      offset: null,
      overwrite: "true",
      revision: initialRevision,
      bodyLength: contents.length,
    },
    {
      method: "PUT",
      uploadId: requests[1].uploadId,
      length: String(contents.length),
      offset: null,
      overwrite: "false",
      revision: null,
      bodyLength: contents.length,
    },
  ]);
  expect(requests[1].uploadId).not.toBe(requests[0].uploadId);
  expect(discards).toEqual([{
    path: logicalPath,
    upload_id: requests[0].uploadId,
  }]);
});

test("超出单批文件上限时不创建上传行", async ({ appPage: page }) => {
  await page.locator("#file").evaluate(input => {
    const transfer = new DataTransfer();
    for (let index = 0; index < 513; index++) {
      transfer.items.add(new File(["x"], `bounded-${index}.txt`));
    }
    input.files = transfer.files;
    input.dispatchEvent(new Event("change", { bubbles: true }));
  });
  await expect(page.locator(".upload-queue-message")).toContainText(
    "Select no more than 512 files in one batch",
  );
  await expect(page.locator(".upload-status")).toHaveCount(0);
  await expect(page.getByRole("dialog", {
    name: "Existing upload destinations",
  })).toBeHidden();
});

test("多个合法批次合计超过全局上限时不创建额外上传行", async ({
  appPage: page,
}) => {
  test.setTimeout(60_000);
  await page.evaluate(() => {
    XMLHttpRequest.prototype.send = function () {
      // Keep the first upload and its queue non-terminal without sending data.
    };
  });
  const firstBatch = Array.from({ length: 260 }, (_, index) => ({
    name: `global-cap-first-${index}.txt`,
    buffer: Buffer.from(String(index)),
    lastModified: 1_724_000_000_000 + index,
  }));
  await selectFiles(page, "#file", firstBatch);
  await expect(page.locator(".upload-status")).toHaveCount(260);

  await page.locator("#file").evaluate(input => {
    const transfer = new DataTransfer();
    for (let index = 0; index < 260; index++) {
      transfer.items.add(new File(["x"], `global-cap-second-${index}.txt`));
    }
    input.files = transfer.files;
    input.dispatchEvent(new Event("change", { bubbles: true }));
  });

  await expect(page.locator(".upload-queue-message")).toContainText(
    "At most 512 uploads may be pending at once",
  );
  await expect(page.locator(".upload-status")).toHaveCount(260);
  await expect(page.locator("#upload260")).toHaveCount(0);
  await expect(page.getByRole("dialog", {
    name: "Existing upload destinations",
  })).toBeHidden();
});

test("终态历史淘汰聚焦行时把焦点移到相邻结果或历史摘要", async ({
  appPage: page,
}) => {
  let releaseSecond;
  let markSecondStarted;
  const secondGate = new Promise(resolve => {
    releaseSecond = resolve;
  });
  const secondStarted = new Promise(resolve => {
    markSecondStarted = resolve;
  });
  await page.route("**/focus-history-*.txt", async route => {
    if (new URL(route.request().url()).pathname.endsWith(
      "/focus-history-second.txt",
    )) {
      markSecondStarted();
      await secondGate;
    }
    await route.fulfill({
      status: 201,
      headers: protocolHeaders(route.request(), "committed", "length"),
      body: "",
    });
  });

  await page.evaluate(async () => {
    const entrypoint = document.querySelector('script[type="module"]').src;
    const { createUploadManager } = await import(
      new URL("modules/upload/manager.js", entrypoint).href
    );
    const encoded = document.querySelector("#index-data").content.textContent;
    const binary = atob(encoded.trim());
    const bytes = Uint8Array.from(
      binary,
      character => character.charCodeAt(0),
    );
    const data = JSON.parse(new TextDecoder().decode(bytes));

    const fixture = document.createElement("section");
    fixture.id = "focus-history-fixture";
    const queueMessage = document.createElement("div");
    const emptyFolder = document.createElement("div");
    const table = document.createElement("table");
    const historyStatus = table.createCaption();
    historyStatus.className = "upload-history-status hidden";
    table.createTHead().insertRow().insertCell().textContent = "Uploads";
    fixture.append(queueMessage, emptyFolder, table);
    document.body.append(fixture);

    window.__dufsFocusHistoryManager = createUploadManager({
      data,
      dialogs: {
        showMessage: async () => undefined,
        chooseAction: async () => {
          throw new Error("unexpected upload confirmation");
        },
      },
      uploadersTable: table,
      queueMessage,
      historyStatus,
      emptyFolder,
      onMutation: () => {},
      onUnauthorized: () => {
        throw new Error("unexpected authentication failure");
      },
      maxConcurrentUploads: 1,
      maxTerminalRows: 1,
    });
  });

  const addFile = name => page.evaluate(async fileName => {
    await window.__dufsFocusHistoryManager.addFiles([
      new File([fileName], fileName, { type: "text/plain" }),
    ]);
  }, name);
  const fixture = page.locator("#focus-history-fixture");
  await addFile("focus-history-first.txt");
  await expect(fixture.locator("#uploadStatus0")).toHaveAttribute(
    "aria-label",
    "focus-history-first.txt: upload complete",
  );
  await fixture.getByRole("link", {
    name: "focus-history-first.txt",
  }).focus();

  await addFile("focus-history-second.txt");
  await secondStarted;
  await fixture.locator("#upload1 .cell-name a").evaluate(link => {
    window.__dufsDetachedFocusHistoryLink = link;
    link.remove();
  });
  releaseSecond();
  const historyStatus = fixture.locator(".upload-history-status");
  await expect(fixture.locator("#upload0")).toHaveCount(0);
  await expect(historyStatus).toBeFocused();
  await expect(historyStatus).toHaveText(
    "1 older upload result hidden; showing the most recent 1.",
  );
  await page.getByRole("button", { name: "Upload files", exact: true }).focus();
  await expect(historyStatus).not.toHaveAttribute("tabindex");

  await fixture.locator("#upload1 .cell-name").evaluate(cell => {
    cell.append(window.__dufsDetachedFocusHistoryLink);
    delete window.__dufsDetachedFocusHistoryLink;
  });
  const second = fixture.getByRole("link", {
    name: "focus-history-second.txt",
  });
  await second.focus();

  await addFile("focus-history-third.txt");
  await expect(fixture.locator("#upload1")).toHaveCount(0);
  await expect(fixture.getByRole("link", {
    name: "focus-history-third.txt",
  })).toBeFocused();
  await expect(historyStatus).toHaveText(
    "2 older upload results hidden; showing the most recent 1.",
  );
});

test("待处理队列已满时保留失败上传的可恢复终态", async ({
  appPage: page,
}) => {
  test.setTimeout(60_000);
  await page.route("**/recovery-at-cap.txt", route => route.fulfill({
    status: 429,
    contentType: "application/problem+json",
    headers: protocolHeaders(route.request(), "not-started"),
    body: problemDetails(
      429,
      "upload_concurrency_limit",
      "Upload was not started",
      "retry",
    ),
  }));
  await selectFiles(page, "#file", [{
    name: "recovery-at-cap.txt",
    buffer: Buffer.from("retry later"),
  }]);
  const retry = page.getByRole("button", {
    name: "Retry upload recovery-at-cap.txt",
  });
  await expect(retry).toBeVisible();
  await expect(page.locator("#uploadStatus0")).toHaveAttribute(
    "aria-label",
    "recovery-at-cap.txt: Upload was not started",
  );

  await page.evaluate(() => {
    XMLHttpRequest.prototype.send = function () {
      // Keep every newly admitted row nonterminal without sending data.
    };
  });
  const pending = Array.from({ length: 512 }, (_, index) => ({
    name: `recovery-cap-pending-${index}.txt`,
    buffer: Buffer.from("x"),
    lastModified: 1_724_100_000_000 + index,
  }));
  await selectFiles(page, "#file", pending);
  await expect(page.locator(".upload-status")).toHaveCount(513, {
    timeout: 20_000,
  });

  await retry.click();
  await expect(page.locator(".upload-queue-message")).toContainText(
    "At most 512 uploads may be pending at once",
  );
  await expect(retry).toBeVisible();
  await expect(page.locator("#uploadStatus0")).toHaveAttribute(
    "aria-label",
    "recovery-at-cap.txt: Upload was not started",
  );
  await expect(page.locator(".upload-status")).toHaveCount(513);
});

test("网络失败后将结果标记未知并冻结后续队列", async ({
  appPage: page,
}) => {
  let secondStarted = false;
  await page.route("**/retry-smoke.txt", async route => {
    await route.abort("connectionfailed");
  });
  await page.route("**/retry-smoke-second.txt", async route => {
    secondStarted = true;
    await route.fulfill({
      status: 201,
      headers: protocolHeaders(route.request(), "committed", "length"),
      body: "",
    });
  });

  await selectFiles(page, "#file", [
    {
      name: "retry-smoke.txt",
      mimeType: "text/plain",
      buffer: Buffer.from("result uncertain"),
      lastModified: 1_722_000_000_301,
    },
    {
      name: "retry-smoke-second.txt",
      mimeType: "text/plain",
      buffer: Buffer.from("must remain queued"),
      lastModified: 1_722_000_000_302,
    },
  ]);
  await expect(page.locator("#uploadStatus0")).toHaveAttribute(
    "aria-label",
    /upload result unknown/,
  );
  await expect(page.locator(".upload-queue-message")).toContainText(
    "remaining upload queue is paused",
  );
  await expect(page.getByRole("button", {
    name: "Refresh",
    exact: true,
  })).toBeVisible();
  await expect(page.locator(".list-status")).toContainText(
    "Folder contents may have changed",
  );
  expect(secondStarted).toBe(false);
  await expect(page.getByRole("button", {
    name: "Retry upload retry-smoke.txt",
  })).toHaveCount(0);
  await expect(page.getByRole("button", {
    name: "Check upload status retry-smoke.txt",
  })).toBeVisible();
});

test("XHR 对超限错误响应立即中止且不读取无界 responseText", async ({
  appPage: page,
}) => {
  await page.route("**/oversized-upload-error.txt", async route => {
    await route.fulfill({
      status: 500,
      contentType: "text/plain",
      headers: protocolHeaders(route.request(), "rejected"),
      body: "x".repeat(20 * 1024),
    });
  });

  await selectFiles(page, "#file", [{
    name: "oversized-upload-error.txt",
    buffer: Buffer.from("upload payload"),
    lastModified: 1_722_000_000_398,
  }]);
  await expect(page.locator(".upload-status")).toHaveAttribute(
    "aria-label",
    /upload result unknown.*response exceeded the allowed size/i,
  );
  await expect(page.locator(".upload-queue-message")).toContainText(
    "remaining upload queue is paused",
  );
  await expect(page.getByRole("button", {
    name: "Check upload status oversized-upload-error.txt",
  })).toBeVisible();
});

test("Retry-After 到期后先查询原会话再按 not-seen 新建", async ({
  appPage: page,
}) => {
  await page.clock.install();
  let putCount = 0;
  let headCount = 0;
  const uploadIds = [];
  await page.route("**/not-started-upload.txt", async route => {
    const request = route.request();
    if (request.method() === "HEAD") {
      headCount++;
      await route.fulfill({
        status: 404,
        headers: protocolHeaders(request, "not-seen"),
        body: "",
      });
      return;
    }
    putCount++;
    uploadIds.push(request.headers()["x-dufs-upload-id"]);
    if (putCount === 1) {
      await route.fulfill({
        status: 429,
        contentType: "application/problem+json",
        headers: {
          ...protocolHeaders(request, "not-started"),
          "Retry-After": "1",
        },
        body: problemDetails(
          429,
          "upload_concurrency_limit",
          "Too many concurrent uploads",
          "retry",
          { retry_after: 9 },
        ),
      });
      return;
    }
    await route.fulfill({
      status: 201,
      headers: protocolHeaders(request, "committed", "length"),
      body: "",
    });
  });

  await selectFiles(page, "#file", [{
    name: "not-started-upload.txt",
    buffer: Buffer.from("retry safely"),
    lastModified: 1_722_000_000_399,
  }]);
  const retry = page.getByRole("button", {
    name: "Retry upload not-started-upload.txt",
  });
  await expect(retry).toBeVisible();
  await expect(retry).toBeDisabled();
  await expect(page.locator(".upload-queue-message")).not.toContainText(
    "remaining upload queue is paused",
  );
  await retry.evaluate(button => {
    button.removeAttribute("disabled");
    button.click();
  });
  expect(putCount).toBe(1);
  await page.clock.fastForward(1_001);
  await expect(retry).toBeEnabled();
  await retry.click();
  await expect(page.locator(".upload-status")).toHaveAttribute(
    "aria-label",
    "not-started-upload.txt: upload complete",
  );
  expect({ putCount, headCount }).toEqual({ putCount: 2, headCount: 1 });
  expect(uploadIds[1]).not.toBe(uploadIds[0]);
});

test("无 recovery 或 refresh_target 时不提供或执行上传重试", async ({
  appPage: page,
}) => {
  await page.route("**/no-upload-recovery.txt", route => route.fulfill({
    status: 409,
    contentType: "application/problem+json",
    headers: protocolHeaders(route.request(), "rejected"),
    body: problemDetails(
      409,
      "upload_rejected",
      "Upload cannot be retried",
    ),
  }));
  await page.route("**/refresh-upload-target.txt", route => route.fulfill({
    status: 409,
    contentType: "application/problem+json",
    headers: protocolHeaders(route.request(), "rejected"),
    body: problemDetails(
      409,
      "upload_target_changed",
      "Refresh the target before continuing",
      "refresh_target",
    ),
  }));

  await selectFiles(page, "#file", [
    {
      name: "no-upload-recovery.txt",
      buffer: Buffer.from("no recovery"),
      lastModified: 1_722_000_000_401,
    },
    {
      name: "refresh-upload-target.txt",
      buffer: Buffer.from("refresh target"),
      lastModified: 1_722_000_000_402,
    },
  ]);
  await expect(page.locator(".upload-failure")).toHaveCount(2);
  await expect(page.locator(".retry-btn")).toHaveCount(0);
});

test("非法 Retry-After 响应头不会回退到正文延迟", async ({
  appPage: page,
}) => {
  await page.route("**/invalid-retry-after.txt", route => route.fulfill({
    status: 429,
    contentType: "application/problem+json",
    headers: {
      ...protocolHeaders(route.request(), "not-started"),
      "Retry-After": "invalid",
    },
    body: problemDetails(
      429,
      "upload_concurrency_limit",
      "Try the upload again",
      "retry",
      { retry_after: 60 },
    ),
  }));

  await selectFiles(page, "#file", [{
    name: "invalid-retry-after.txt",
    buffer: Buffer.from("retry metadata"),
    lastModified: 1_722_000_000_403,
  }]);
  await expect(page.getByRole("button", {
    name: "Retry upload invalid-retry-after.txt",
  })).toBeEnabled();
});

test("Problem status 冲突时进入 unknown 且不采纳重放 recovery", async ({
  appPage: page,
}) => {
  let requestCount = 0;
  await page.route("**/mismatched-problem-status.txt", route => {
    requestCount++;
    return route.fulfill({
      status: 409,
      contentType: "application/problem+json",
      headers: protocolHeaders(route.request(), "rejected"),
      body: problemDetails(
        500,
        "upload_precommit_failed",
        "Conflicting error status",
        "retry_with_new_id",
      ),
    });
  });

  await selectFiles(page, "#file", [{
    name: "mismatched-problem-status.txt",
    buffer: Buffer.from("do not replay"),
    lastModified: 1_722_000_000_405,
  }]);
  await expect(page.locator(".upload-unknown")).toContainText(
    "problem status does not match HTTP status",
  );
  await expect(page.locator(".retry-btn")).toHaveCount(0);
  expect(requestCount).toBe(1);
});

test("重试 HEAD 缺少绑定长度时暂停队列且不启动新会话", async ({
  appPage: page,
}) => {
  let putCount = 0;
  let headCount = 0;
  await page.route("**/malformed-retry-status.txt", async route => {
    const request = route.request();
    if (request.method() === "HEAD") {
      headCount++;
      await route.fulfill({
        status: 409,
        headers: protocolHeaders(request, "rejected"),
        body: "",
      });
      return;
    }
    putCount++;
    await route.fulfill({
      status: 409,
      contentType: "application/problem+json",
      headers: protocolHeaders(request, "rejected"),
      body: problemDetails(
        409,
        "upload_id_rejected",
        "known rejection",
        "query_upload",
      ),
    });
  });

  await selectFiles(page, "#file", [{
    name: "malformed-retry-status.txt",
    buffer: Buffer.from("retry binding"),
    lastModified: 1_722_000_000_400,
  }]);
  const retry = page.getByRole("button", {
    name: "Check upload status malformed-retry-status.txt",
  });
  await expect(retry).toBeVisible();
  await retry.click();
  await expect(page.locator("#uploadStatus0")).toHaveAttribute(
    "aria-label",
    /upload result unknown/,
  );
  await expect(page.locator(".upload-queue-message")).toContainText(
    "remaining upload queue is paused",
  );
  expect({ putCount, headCount }).toEqual({ putCount: 1, headCount: 1 });
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
    name: "Retry upload resume-with-patch.txt",
  });
  await expect(retry).toBeVisible();
  await retry.click();
  await expect(page.locator(".upload-status")).toHaveAttribute(
    "aria-label",
    "resume-with-patch.txt: upload complete",
  );
  expect(methods).toEqual(["PUT", "HEAD", "PATCH"]);
  expect(offsets).toEqual([null, null, "4"]);
  expect(
    await page.evaluate(
      async url => (await fetch(url)).text(),
      currentUrl(page, "resume-with-patch.txt"),
    ),
  ).toBe("AAAABBBB");
});

test("状态查询发现完整 running 检查点时用空 PATCH 重入提交", async ({
  appPage: page,
}) => {
  const requests = [];
  await page.route("**/resume-full-checkpoint.txt", route => {
    const request = route.request();
    requests.push({
      method: request.method(),
      offset: request.headers()["x-dufs-upload-offset"] || null,
      bodyLength: request.postDataBuffer()?.length || 0,
    });
    if (request.method() === "PUT") {
      return route.fulfill({
        status: 408,
        contentType: "application/problem+json",
        headers: protocolHeaders(request, "unknown", "length"),
        body: problemDetails(
          408,
          "upload_outcome_unknown",
          "Upload result is unknown",
          "query_upload",
        ),
      });
    }
    if (request.method() === "HEAD") {
      return route.fulfill({
        status: 200,
        headers: protocolHeaders(request, "running", 8, 8),
        body: "",
      });
    }
    expect(request.method()).toBe("PATCH");
    return route.fulfill({
      status: 204,
      headers: protocolHeaders(request, "committed", "length"),
      body: "",
    });
  });

  await selectFiles(page, "#file", [{
    name: "resume-full-checkpoint.txt",
    buffer: Buffer.from("AAAABBBB"),
    lastModified: 1_722_000_000_303,
  }]);
  const query = page.getByRole("button", {
    name: "Check upload status resume-full-checkpoint.txt",
  });
  await expect(query).toBeVisible();
  await query.click();
  await expect(page.locator(".upload-status")).toHaveAttribute(
    "aria-label",
    "resume-full-checkpoint.txt: upload complete",
  );
  await expect(page.locator("#upload0 .cell-name a")).toBeFocused();
  expect(requests).toEqual([
    { method: "PUT", offset: null, bodyLength: 8 },
    { method: "HEAD", offset: null, bodyLength: 0 },
    { method: "PATCH", offset: "8", bodyLength: 0 },
  ]);
});

test("上传槽占用时重试会排队而不是静默失效", async ({
  appPage: page,
}) => {
  let firstPut = true;
  const firstUploadIds = [];
  const firstMethods = [];
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
      firstMethods.push(request.method());
      firstUploadIds.push(request.headers()["x-dufs-upload-id"]);
      if (request.method() === "PUT" && firstPut) {
        firstPut = false;
        await route.fulfill({
          status: 409,
          contentType: "application/problem+json",
          headers: protocolHeaders(request, "rejected"),
          body: problemDetails(
            409,
            "upload_id_rejected",
            "known rejection",
            "retry_with_new_id",
          ),
        });
      } else if (request.method() === "HEAD") {
        await route.fulfill({
          status: 409,
          headers: protocolHeaders(request, "rejected", 0, 5),
          body: "",
        });
      } else {
        await route.fulfill({
          status: 201,
          headers: protocolHeaders(request, "committed", "length"),
          body: "",
        });
      }
      return;
    }
    markSecondStarted();
    await secondGate;
    await route.fulfill({
      status: 201,
      headers: protocolHeaders(request, "committed", "length"),
      body: "",
    });
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
    name: "Retry upload queue-retry-first.txt",
  });
  await expect(retry).toBeVisible();
  await secondStarted;
  await retry.click();
  await expect(page.locator("#uploadStatus0")).toHaveAttribute(
    "aria-label",
    "queue-retry-first.txt: waiting to retry",
  );
  releaseSecond();
  await expect(
    page.locator('.upload-status[aria-label$="upload complete"]'),
  ).toHaveCount(2);
  expect(firstMethods).toEqual(["PUT", "HEAD", "PUT"]);
  expect(firstUploadIds[1]).toBe(firstUploadIds[0]);
  expect(firstUploadIds[2]).not.toBe(firstUploadIds[0]);
});

test("取消只读续传状态查询不会误报提交结果未知", async ({
  appPage: page,
}) => {
  let releaseStatus;
  const statusGate = new Promise(resolve => {
    releaseStatus = resolve;
  });
  await page.route("**/cancel-resume-check.txt", async route => {
    const request = route.request();
    if (request.method() === "PUT") {
      await route.fulfill({
        status: 409,
        contentType: "application/problem+json",
        headers: protocolHeaders(request, "rejected"),
        body: problemDetails(
          409,
          "upload_id_rejected",
          "known rejection",
          "query_upload",
        ),
      });
      return;
    }
    await statusGate;
    try {
      await route.fulfill({
        status: 409,
        headers: protocolHeaders(request, "rejected", 0, 12),
        body: "",
      });
    } catch {
      // Cancelling the read-only HEAD may close the intercepted request.
    }
  });

  await selectFiles(page, "#file", [{
    name: "cancel-resume-check.txt",
    buffer: Buffer.from("retry status"),
    lastModified: 1_722_000_000_312,
  }]);
  const retry = page.getByRole("button", {
    name: "Check upload status cancel-resume-check.txt",
  });
  await expect(retry).toBeVisible();
  await retry.click();
  const cancel = page.getByRole("button", {
    name: "Cancel resume status check for cancel-resume-check.txt",
  });
  await expect(cancel).toBeVisible();
  await expect(cancel).toBeFocused();
  await cancel.click();
  releaseStatus();

  await expect(page.locator("#uploadStatus0")).toContainText(
    "Upload cancelled",
  );
  await expect(page.locator("#uploadStatus0 .upload-unknown")).toHaveCount(0);
  await expect(page.getByRole("button", {
    name: "Check upload status cancel-resume-check.txt",
  })).toBeVisible();
  await expect(page.locator(".upload-queue-message")).not.toContainText(
    "remaining upload queue is paused",
  );
});

test("上传进度更新保留取消按钮焦点且发送后的取消保守标记未知", async ({
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
      window.__dufsTestDispatchUpload = () => originalSend.apply(this, args);
    };
  });

  await selectFiles(page, "#file", [{
    name: "cancel-focus.txt",
    buffer: Buffer.from("cancel me"),
    lastModified: 1_722_000_000_305,
  }]);
  const cancel = page.getByRole("button", {
    name: "Cancel upload cancel-focus.txt",
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
  await expect(page.locator("#uploadStatus0 .upload-speed")).toBeVisible();
  await expect(page.locator("#uploadStatus0 .upload-speed")).toContainText(
    "/s",
  );
  await expect(page.locator("#uploadStatus0 .upload-progress")).toContainText(
    "44.4%",
  );
  expect(
    await page.evaluate(() =>
      document.activeElement === window.__dufsTestCancelButton &&
      window.__dufsTestCancelButton.isConnected
    ),
  ).toBe(true);
  await page.evaluate(() => {
    window.__dufsTestDispatchUpload();
    // A routed request can hold the browser before it emits the native upload
    // load event. This test controls that boundary explicitly so it exercises
    // the post-body cancellation state rather than Playwright routing timing.
    window.__dufsTestUploadRequest.upload.dispatchEvent(
      new ProgressEvent("load"),
    );
  });
  const stopWaiting = page.getByRole("button", {
    name: "Stop waiting for upload cancel-focus.txt",
  });
  await expect(stopWaiting).toBeVisible();
  await expect(stopWaiting).toBeFocused();
  await stopWaiting.click();
  releaseRequest();
  await expect(page.locator(".upload-unknown")).toContainText(
    "server result is unknown",
  );
  await expect(page.getByRole("button", {
    name: "Check upload status cancel-focus.txt",
  })).toBeFocused();
});

test("正文发送后停止空闲计时并将提交确认超时标记为结果未知", async ({
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
  await page.evaluate(() => {
    const originalSend = XMLHttpRequest.prototype.send;
    XMLHttpRequest.prototype.send = function (...args) {
      window.__dufsTestUploadRequest = this;
      return originalSend.apply(this, args);
    };
  });

  await selectFiles(page, "#file", [{
    name: "idle-timeout.txt",
    buffer: Buffer.from("idle"),
    lastModified: 1_722_000_000_306,
  }]);
  await page.waitForFunction(() => Boolean(window.__dufsTestUploadRequest));
  await page.evaluate(() => {
    window.__dufsTestUploadRequest.upload.dispatchEvent(new Event("load"));
  });
  const status = page.locator(".upload-status");
  await expect(status).toHaveAttribute(
    "aria-label",
    "idle-timeout.txt: upload data sent; waiting for server confirmation",
  );
  await expect(page.getByRole("button", {
    name: "Stop waiting for upload idle-timeout.txt",
  })).toBeVisible();
  await page.clock.fastForward(2 * 60 * 1000 + 1);
  await expect(status).toHaveAttribute(
    "aria-label",
    "idle-timeout.txt: upload data sent; waiting for server confirmation",
  );
  await page.clock.fastForward(3 * 60 * 1000 + 1);
  await expect(page.locator(".upload-unknown")).toContainText(
    "The result is unknown",
  );
  await expect(status).toHaveAttribute(
    "aria-label",
    /upload result unknown/,
  );
  await expect(page.getByRole("button", {
    name: "Retry upload idle-timeout.txt",
  })).toHaveCount(0);
  await expect(page.getByRole("button", {
    name: "Check upload status idle-timeout.txt",
  })).toBeVisible();
  releaseRequest();
});

test("服务端提交结果未知时不提供上传重试", async ({ appPage: page }) => {
  await page.route("**/server-unknown.txt", route => route.fulfill({
    status: 500,
    contentType: "application/problem+json",
    headers: protocolHeaders(route.request(), "unknown", "length"),
    body: problemDetails(
      500,
      "outcome_uncertain",
      "Upload commit outcome is uncertain",
      "query_upload",
      {
        upload_id: route.request().headers()["x-dufs-upload-id"],
        upload_state: "unknown",
        upload_length: Number(
          route.request().headers()["x-dufs-upload-length"],
        ),
        upload_offset: Number(
          route.request().headers()["x-dufs-upload-length"],
        ),
      },
    ),
  }));

  await selectFiles(page, "#file", [{
    name: "server-unknown.txt",
    buffer: Buffer.from("uncertain"),
    lastModified: 1_722_000_000_307,
  }]);
  await expect(page.locator(".upload-unknown")).toContainText(
    "Upload commit outcome is uncertain",
  );
  await expect(page.locator(".upload-status")).toHaveAttribute(
    "aria-label",
    /upload result unknown/,
  );
  await expect(page.getByRole("button", {
    name: "Retry upload server-unknown.txt",
  })).toHaveCount(0);
});

test("unknown 的 query_upload 只查询状态而不重放上传", async ({
  appPage: page,
}) => {
  const methods = [];
  await page.route("**/query-unknown-upload.txt", route => {
    const request = route.request();
    methods.push(request.method());
    if (request.method() === "HEAD") {
      return route.fulfill({
        status: 200,
        headers: protocolHeaders(request, "running", 4, 8),
        body: "",
      });
    }
    return route.fulfill({
      status: 408,
      contentType: "application/problem+json",
      headers: protocolHeaders(request, "unknown", "length"),
      body: problemDetails(
        408,
        "upload_outcome_unknown",
        "Upload result is unknown",
        "query_upload",
      ),
    });
  });

  await selectFiles(page, "#file", [{
    name: "query-unknown-upload.txt",
    buffer: Buffer.from("AAAABBBB"),
    lastModified: 1_722_000_000_404,
  }]);
  const query = page.getByRole("button", {
    name: "Check upload status query-unknown-upload.txt",
  });
  await expect(query).toBeVisible();
  await query.click();
  await expect(page.getByRole("button", {
    name: "Retry upload query-unknown-upload.txt",
  })).toBeVisible();
  expect(methods).toEqual(["PUT", "HEAD"]);
  await expect(page.locator(".upload-queue-message")).toBeHidden();
});

test("状态查询返回持久 unknown 时停止重复查询", async ({ appPage: page }) => {
  const methods = [];
  await page.route("**/persisted-unknown-upload.txt", route => {
    const request = route.request();
    methods.push(request.method());
    if (request.method() === "HEAD") {
      return route.fulfill({
        status: 500,
        contentType: "application/problem+json",
        headers: protocolHeaders(request, "unknown", 8, 8),
        body: problemDetails(
          500,
          "upload_publication_outcome_unknown",
          "Upload publication outcome is unknown",
          "query_upload",
        ),
      });
    }
    return route.fulfill({
      status: 500,
      contentType: "application/problem+json",
      headers: protocolHeaders(request, "unknown", 8, 8),
      body: problemDetails(
        500,
        "upload_outcome_unknown",
        "Upload result is unknown",
        "query_upload",
      ),
    });
  });

  await selectFiles(page, "#file", [{
    name: "persisted-unknown-upload.txt",
    buffer: Buffer.from("AAAABBBB"),
    lastModified: 1_722_000_000_405,
  }]);
  const query = page.getByRole("button", {
    name: "Check upload status persisted-unknown-upload.txt",
  });
  await expect(query).toBeVisible();
  await query.click();
  await expect(page.locator(".upload-unknown")).toContainText(
    "recorded an uncertain publication outcome",
  );
  await expect(page.getByRole("button", {
    name: "Check upload status persisted-unknown-upload.txt",
  })).toHaveCount(0);
  expect(methods).toEqual(["PUT", "HEAD"]);
});

test("重试状态查询发现已提交终态时不会再次 PUT", async ({ appPage: page }) => {
  let putCount = 0;
  await page.route("**/terminal-replay.txt", route => {
    const request = route.request();
    if (request.method() === "HEAD") {
      return route.fulfill({
        status: 200,
        headers: protocolHeaders(request, "committed", 8, 8),
        body: "",
      });
    }
    putCount++;
    return route.fulfill({
      status: 409,
      contentType: "application/problem+json",
      headers: protocolHeaders(request, "running", 4),
      body: problemDetails(
        409,
        "upload_in_progress",
        "query the checkpoint",
        "query_upload",
      ),
    });
  });

  await selectFiles(page, "#file", [{
    name: "terminal-replay.txt",
    buffer: Buffer.from("terminal"),
    lastModified: 1_722_000_000_308,
  }]);
  const retry = page.getByRole("button", {
    name: "Check upload status terminal-replay.txt",
  });
  await expect(retry).toBeVisible();
  await retry.click();
  await expect(page.locator(".upload-status")).toHaveAttribute(
    "aria-label",
    "terminal-replay.txt: upload complete",
  );
  expect(putCount).toBe(1);
});

test("成功状态缺少绑定头时标记未知而不是误报完成", async ({
  appPage: page,
}) => {
  await page.route("**/invalid-success.txt", route => route.fulfill({
    status: 201,
    body: "",
  }));
  await selectFiles(page, "#file", [{
    name: "invalid-success.txt",
    buffer: Buffer.from("invalid"),
    lastModified: 1_722_000_000_309,
  }]);
  await expect(page.locator(".upload-unknown")).toContainText(
    "inconsistent upload response",
  );
});

test("同一页面重复逻辑目标只会排入一次", async ({ appPage: page }) => {
  let putCount = 0;
  await page.route("**/duplicate-target.txt", route => {
    putCount++;
    return route.fulfill({
      status: 201,
      headers: protocolHeaders(route.request(), "committed", "length"),
      body: "",
    });
  });
  await selectFiles(page, "#file", [
    {
      name: "duplicate-target.txt",
      buffer: Buffer.from("first"),
      lastModified: 1_722_000_000_310,
    },
    {
      name: "duplicate-target.txt",
      buffer: Buffer.from("second"),
      lastModified: 1_722_000_000_311,
    },
  ]);
  await expect(page.locator(".upload-status")).toHaveCount(1);
  await expect(page.locator(".upload-status")).toHaveAttribute(
    "aria-label",
    "duplicate-target.txt: upload complete",
  );
  await expect(page.locator(".upload-queue-message")).toContainText(
    "Skipped duplicate upload target duplicate-target.txt",
  );
  expect(putCount).toBe(1);
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
    "Your session or this page is no longer valid. Refresh the page and select the files again.",
  );
  // Preflight fails before the batch is admitted, so no misleading pending
  // rows are created for files that were never sent.
  await expect(page.locator(".upload-status")).toHaveCount(0);

  let beforeUnloadDialogs = 0;
  page.on("dialog", async dialog => {
    if (dialog.type() === "beforeunload") beforeUnloadDialogs++;
    await dialog.accept();
  });
  await page.reload();
  await expect(page.locator(".index-page")).toBeVisible();
  expect(beforeUnloadDialogs).toBe(0);

  await selectFiles(page, "#file", files);
  await expect(page.locator('.upload-status[aria-label$="upload complete"]')).toHaveCount(2);
});

test("刷新后不会按同名同大小同 mtime 自动续传另一份内容", async ({
  appPage: page,
}) => {
  const target = currentLogicalChild(page, "resume-identity.txt");
  const oldUploadId = randomUUID();
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
    page.getByRole("button", { name: "Upload files", exact: true }),
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
    "resume-identity.txt: upload complete",
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
    headers: protocolHeaders(route.request(), "committed", "length"),
    body: "",
  }));
  await page.evaluate(() => localStorage.clear());
  const files = Array.from({ length: 129 }, (_, index) => ({
    name: `bulk-record-${index}.txt`,
    buffer: Buffer.from(String(index)),
    lastModified: 1_723_000_000_000 + index,
  }));
  await selectFiles(page, "#file", files);
  // The product intentionally serializes these 129 uploads. Keep the extra
  // renderer wait local to this stress assertion rather than widening defaults.
  await expect(
    page.locator('.upload-status[aria-label$="upload complete"]'),
  ).toHaveCount(129, { timeout: 30_000 });
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
    "picked-folder/child/nested.txt: upload complete",
  );
  expect(
    await page.evaluate(
      async url => (await fetch(url)).text(),
      currentUrl(page, "picked-folder/child/nested.txt"),
    ),
  ).toBe("folder content");
});

test("等待中的上传可取消且不会阻塞后续重新选择", async ({
  appPage: page,
}) => {
  let releaseFirst;
  const firstGate = new Promise(resolve => {
    releaseFirst = resolve;
  });
  let queuedRequests = 0;
  await page.route("**/cancel-queued-first.txt", async route => {
    await firstGate;
    await route.fulfill({
      status: 201,
      headers: protocolHeaders(route.request(), "committed", "length"),
      body: "",
    });
  });
  await page.route("**/cancel-queued-second.txt", route => {
    queuedRequests++;
    return route.fulfill({
      status: 201,
      headers: protocolHeaders(route.request(), "committed", "length"),
      body: "",
    });
  });
  const secondFile = {
    name: "cancel-queued-second.txt",
    buffer: Buffer.from("second"),
    lastModified: 1_722_000_000_402,
  };
  await selectFiles(page, "#file", [
    {
      name: "cancel-queued-first.txt",
      buffer: Buffer.from("first"),
      lastModified: 1_722_000_000_401,
    },
    secondFile,
  ]);
  await page.getByRole("button", {
    name: "Cancel queued upload cancel-queued-second.txt",
  }).click();
  await expect(page.locator("#upload1 .cell-name a")).toBeFocused();
  await expect(page.locator("#uploadStatus1")).toHaveAttribute(
    "aria-label",
    "cancel-queued-second.txt: upload cancelled",
  );
  releaseFirst();
  await expect(page.locator("#uploadStatus0")).toHaveAttribute(
    "aria-label",
    "cancel-queued-first.txt: upload complete",
  );
  expect(queuedRequests).toBe(0);

  await selectFiles(page, "#file", [secondFile]);
  await expect(page.locator("#uploadStatus2")).toHaveAttribute(
    "aria-label",
    "cancel-queued-second.txt: upload complete",
  );
  expect(queuedRequests).toBe(1);
});
