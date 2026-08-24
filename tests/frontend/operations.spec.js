const { randomUUID } = require("node:crypto");
const {
  actionDialog,
  currentDirectoryPath,
  currentLogicalChild,
  currentUrl,
  expect,
  pageData,
  rotateSession,
  rowByName,
  selectFiles,
  submitActionDialog,
  test,
} = require("./fixtures");

const UUID_V4_PATTERN =
  /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;

function discardProtocolHeaders(uploadId, length, offset = length) {
  return {
    "X-Dufs-Upload-Id": uploadId,
    "X-Dufs-Upload-Length": String(length),
    "X-Dufs-Upload-Offset": String(offset),
    "X-Dufs-Operation-State": "rejected",
  };
}

function fulfillCreatedItemPreflight(route) {
  const { paths } = route.request().postDataJSON();
  return route.fulfill({
    status: 200,
    contentType: "application/json",
    body: JSON.stringify({
      targets: paths.map(path => ({
        path,
        exists: true,
        revision: "d".repeat(64),
        replaceable: true,
      })),
    }),
  });
}

function fulfillTrackedFailure(route, status, code, detail, headers = {}) {
  const operationId = route.request().headers()["x-dufs-operation-id"];
  return route.fulfill({
    status,
    contentType: "application/problem+json",
    headers: {
      "X-Dufs-Operation-Id": operationId,
      "X-Dufs-Operation-State": "failed",
      ...headers,
    },
    body: JSON.stringify({
      type: `urn:dufs:problem:${code}`,
      title: "Precondition failed",
      status,
      detail,
      code,
    }),
  });
}

function fulfillDirectoryChanged(route) {
  return route.fulfill({
    status: 409,
    contentType: "application/problem+json",
    body: JSON.stringify({
      type: "urn:dufs:problem:directory_changed",
      title: "Directory changed",
      status: 409,
      detail: "Directory changed; restart listing",
      code: "directory_changed",
      recovery: "refresh_target",
    }),
  });
}

function inlineNameInput(page) {
  return page.locator(".inline-name-input");
}

async function expectSelection(input, start, end) {
  expect(await input.evaluate(element => ({
    start: element.selectionStart,
    end: element.selectionEnd,
  }))).toEqual({ start, end });
}

async function seedFolder(page, name) {
  const data = await pageData(page);
  const root = currentDirectoryPath(page);
  const response = await page.context().request.post(
    new URL("/__dufs__/api/mkdir", page.url()).href,
    {
      headers: {
        "Content-Type": "application/json",
        "X-Dufs-CSRF-Token": data.csrf_token,
        "X-Dufs-Operation-Id": randomUUID(),
      },
      data: { path: `${root}/${name}` },
    },
  );
  expect(response.status()).toBe(201);
}

async function seedEmptyFile(page, name) {
  const data = await pageData(page);
  const response = await page.context().request.put(currentUrl(page, name), {
    headers: {
      "X-Dufs-CSRF-Token": data.csrf_token,
      "X-Dufs-Upload-Id": randomUUID(),
      "X-Dufs-Upload-Length": "0",
      "X-Dufs-Upload-Overwrite": "false",
    },
    data: Buffer.alloc(0),
  });
  expect(response.status()).toBe(201);
}

test("成功响应必须与操作终态协议一致", async ({ appPage: page }) => {
  const results = await page.evaluate(async () => {
    const entrypoint =
      document.querySelector('script[type="module"]').src;
    const apiUrl = new URL("modules/http/client.js", entrypoint).href;
    const {
      assertResponse,
      OPERATION_ID_HEADER,
      OPERATION_STATE_HEADER,
    } = await import(apiUrl);
    const operationId = "00000000-0000-4000-8000-000000000001";
    const check = async (status, state) => {
      const headers = new Headers({
        [OPERATION_ID_HEADER]: operationId,
        [OPERATION_STATE_HEADER]: state,
      });
      try {
        await assertResponse(new Response(null, { status, headers }));
        return { accepted: true };
      } catch (error) {
        return {
          accepted: false,
          code: error.code,
          outcomeUnknown: error.outcomeUnknown,
          operationState: error.operationState,
        };
      }
    };
    return {
      succeeded: await check(204, "succeeded"),
      unknown: await check(204, "unknown"),
      failed: await check(204, "failed"),
      rejected: await check(409, "rejected"),
      running: await check(204, "running"),
      contradictoryError: await check(409, "succeeded"),
    };
  });

  expect(results.succeeded).toEqual({ accepted: true });
  for (const result of [
    results.unknown,
    results.failed,
    results.running,
    results.contradictoryError,
  ]) {
    expect(result).toEqual({
      accepted: false,
      code: "invalid_operation_result",
      outcomeUnknown: true,
      operationState: "unknown",
    });
  }
  expect(results.rejected).toEqual({
    accepted: false,
    code: "",
    outcomeUnknown: false,
    operationState: "rejected",
  });

  await page.route("**/__dufs__/protocol-operation-id", route =>
    route.fulfill({
      status: 204,
      headers: {
        "X-Dufs-Operation-Id":
          "00000000-0000-4000-8000-000000000002",
        "X-Dufs-Operation-State": "succeeded",
      },
    }));
  const mismatchedId = await page.evaluate(async () => {
    const entrypoint =
      document.querySelector('script[type="module"]').src;
    const { requestNoContent } = await import(
      new URL("modules/http/client.js", entrypoint).href
    );
    try {
      await requestNoContent("/__dufs__/protocol-operation-id", {
        method: "POST",
      }, {
        operationId: "00000000-0000-4000-8000-000000000001",
        outcomeUnknown: true,
      });
      return { accepted: true };
    } catch (error) {
      return {
        accepted: false,
        code: error.code,
        operationId: error.operationId,
        operationState: error.operationState,
        outcomeUnknown: error.outcomeUnknown,
      };
    }
  });
  expect(mismatchedId).toEqual({
    accepted: false,
    code: "invalid_operation_result",
    operationId: "00000000-0000-4000-8000-000000000001",
    operationState: "unknown",
    outcomeUnknown: true,
  });

  const boundOperationId = "00000000-0000-4000-8000-000000000003";
  await page.route("**/__dufs__/protocol-error-known", route =>
    route.fulfill({
      status: 409,
      contentType: "application/problem+json",
      headers: {
        "X-Dufs-Operation-Id": boundOperationId,
        "X-Dufs-Operation-State": "failed",
      },
      body: JSON.stringify({
        type: "urn:dufs:problem:known_failure",
        title: "Conflict",
        status: 409,
        code: "known_failure",
        detail: "Known failure",
      }),
    }));
  await page.route("**/__dufs__/protocol-error-missing", route =>
    route.fulfill({ status: 502, body: "gateway failure" }));
  const errorBindings = await page.evaluate(async () => {
    const entrypoint =
      document.querySelector('script[type="module"]').src;
    const { assertResponse, requestNoContent } = await import(
      new URL("modules/http/client.js", entrypoint).href
    );
    const operationId = "00000000-0000-4000-8000-000000000003";
    const invoke = async path => {
      try {
        const response = await requestNoContent(path, { method: "POST" }, {
          operationId,
          outcomeUnknown: true,
        });
        await assertResponse(response);
        return { accepted: true };
      } catch (error) {
        return {
          code: error.code,
          outcomeUnknown: error.outcomeUnknown,
          operationState: error.operationState,
        };
      }
    };
    return {
      missing: await invoke("/__dufs__/protocol-error-missing"),
      known: await invoke("/__dufs__/protocol-error-known"),
    };
  });
  expect(errorBindings.missing).toEqual({
    code: "invalid_operation_result",
    outcomeUnknown: true,
    operationState: "unknown",
  });
  expect(errorBindings.known).toEqual({
    code: "known_failure",
    outcomeUnknown: false,
    operationState: "failed",
  });
});

test("空文件 PUT 只接受合法状态与完整上传状态矩阵", async ({
  appPage: page,
}) => {
  const results = await page.evaluate(async () => {
    const entrypoint = document.querySelector('script[type="module"]').src;
    const {
      assertFreshUploadResponse,
    } = await import(new URL("modules/http/client.js", entrypoint).href);
    const {
      FRESH_UPLOAD_SUCCESS_STATUSES,
      RESUME_UPLOAD_SUCCESS_STATUSES,
      UPLOAD_PROTOCOL_STATES,
      parseBoundUploadProtocol,
    } = await import(
      new URL("modules/upload/protocol.js", entrypoint).href
    );
    const uploadId = "00000000-0000-4000-8000-000000000101";
    const invoke = async (status, state, options = {}) => {
      const headers = new Headers({
        "X-Dufs-Upload-Id": options.uploadId ?? uploadId,
        "X-Dufs-Operation-State": state,
      });
      if (options.length !== null) {
        headers.set("X-Dufs-Upload-Length", options.length ?? "0");
      }
      if (options.offset !== null) {
        headers.set("X-Dufs-Upload-Offset", options.offset ?? "0");
      }
      try {
        await assertFreshUploadResponse(
          new Response(status >= 400 ? "protocol detail" : null, {
            status,
            headers,
          }),
          uploadId,
          0,
        );
        return { accepted: true };
      } catch (error) {
        return {
          accepted: false,
          code: error.code,
          outcomeUnknown: error.outcomeUnknown,
          operationState: error.operationState,
        };
      }
    };
    const mismatchedRunningHeaders = new Headers({
      "X-Dufs-Upload-Id": uploadId,
      "X-Dufs-Operation-State": "running",
      "X-Dufs-Upload-Length": "1",
      "X-Dufs-Upload-Offset": "1",
    });
    return {
      sharedProtocol: {
        states: UPLOAD_PROTOCOL_STATES,
        freshSuccess: FRESH_UPLOAD_SUCCESS_STATUSES,
        resumeSuccess: RESUME_UPLOAD_SUCCESS_STATUSES,
        strictlyParsed: parseBoundUploadProtocol(
          mismatchedRunningHeaders,
          uploadId,
          0,
        ),
        prototypeState: parseBoundUploadProtocol(new Headers({
          "X-Dufs-Upload-Id": uploadId,
          "X-Dufs-Operation-State": "toString",
        }), uploadId, 0),
      },
      ok200: await invoke(200, "committed"),
      created201: await invoke(201, "committed"),
      accepted202: await invoke(202, "committed"),
      noContent204: await invoke(204, "committed"),
      partial206: await invoke(206, "committed"),
      notStarted403: await invoke(403, "not-started", { offset: null }),
      notStarted404: await invoke(404, "not-started", { offset: null }),
      notStarted408: await invoke(408, "not-started", { offset: null }),
      notStarted429: await invoke(429, "not-started", { offset: null }),
      rejected408: await invoke(408, "rejected", { offset: null }),
      rejected409: await invoke(409, "rejected", { offset: null }),
      rejected413: await invoke(413, "rejected", { offset: null }),
      rejected500: await invoke(500, "rejected", { offset: null }),
      rejected507: await invoke(507, "rejected", { offset: null }),
      awaiting409: await invoke(409, "awaiting-confirmation"),
      awaiting408: await invoke(408, "awaiting-confirmation"),
      running409: await invoke(409, "running"),
      unknown408: await invoke(408, "unknown", {
        length: null,
        offset: null,
      }),
      unknown500: await invoke(500, "unknown", {
        length: null,
        offset: null,
      }),
      unknown504: await invoke(504, "unknown", {
        length: null,
        offset: null,
      }),
      notSeen404: await invoke(404, "not-seen", {
        length: null,
        offset: null,
      }),
      notSeen409: await invoke(409, "not-seen", {
        length: null,
        offset: null,
      }),
      notStarted418: await invoke(418, "not-started", { offset: null }),
      notStarted500: await invoke(500, "not-started", { offset: null }),
      rejected429: await invoke(429, "rejected", { offset: null }),
      running408: await invoke(408, "running"),
      committedError: await invoke(409, "committed"),
      missingOffset: await invoke(201, "committed", { offset: null }),
      mismatchedLength: await invoke(201, "committed", { length: "1" }),
    };
  });

  expect(results.sharedProtocol).toEqual({
    states: [
      "running",
      "awaiting-confirmation",
      "committed",
      "rejected",
      "not-seen",
      "not-started",
      "unknown",
    ],
    freshSuccess: [200, 201],
    resumeSuccess: [200, 204],
    strictlyParsed: null,
    prototypeState: null,
  });
  expect(results.ok200).toEqual({ accepted: true });
  expect(results.created201).toEqual({ accepted: true });
  for (const key of [
    "accepted202",
    "noContent204",
    "partial206",
    "notSeen409",
    "notStarted418",
    "notStarted500",
    "rejected429",
    "awaiting408",
    "committedError",
    "missingOffset",
    "mismatchedLength",
  ]) {
    expect(results[key]).toEqual({
      accepted: false,
      code: "invalid_upload_result",
      outcomeUnknown: true,
      operationState: "unknown",
    });
  }
  for (const key of [
    "notStarted403",
    "notStarted404",
    "notStarted408",
    "notStarted429",
    "rejected408",
    "rejected409",
    "rejected413",
    "rejected500",
    "rejected507",
    "awaiting409",
    "notSeen404",
  ]) {
    expect(results[key]).toMatchObject({
      accepted: false,
      outcomeUnknown: false,
    });
  }
  for (const key of [
    "notStarted403",
    "notStarted404",
    "notStarted408",
    "notStarted429",
  ]) {
    expect(results[key].operationState).toBe("not-started");
  }
  for (const key of [
    "rejected408",
    "rejected409",
    "rejected413",
    "rejected500",
    "rejected507",
  ]) {
    expect(results[key].operationState).toBe("rejected");
  }
  expect(results.notSeen404.operationState).toBe("not-seen");
  expect(results.awaiting409.operationState).toBe("awaiting-confirmation");
  for (const key of [
    "running408",
    "running409",
    "unknown408",
    "unknown500",
    "unknown504",
  ]) {
    expect(results[key]).toMatchObject({
      accepted: false,
      outcomeUnknown: true,
    });
  }
});

test("响应体按字节有界读取、取消超限流并保留 Response API", async ({
  appPage: page,
}) => {
  const results = await page.evaluate(async () => {
    const entrypoint = document.querySelector('script[type="module"]').src;
    const {
      ERROR_RESPONSE_BODY_LIMIT,
      SUCCESS_RESPONSE_BODY_LIMIT,
      bufferResponse,
    } = await import(new URL("modules/http/client.js", entrypoint).href);
    const inspectFailure = async response => {
      try {
        await bufferResponse(response);
        return { accepted: true };
      } catch (error) {
        return { accepted: false, code: error.code };
      }
    };

    let declaredCancelled = false;
    const declaredStream = new ReadableStream({
      pull(controller) {
        controller.enqueue(new Uint8Array([1]));
      },
      cancel() {
        declaredCancelled = true;
      },
    });
    const declared = await inspectFailure(new Response(declaredStream, {
      status: 500,
      headers: {
        "Content-Length": String(ERROR_RESPONSE_BODY_LIMIT + 1),
      },
    }));

    let streamedCancelled = false;
    let pullCount = 0;
    const streamed = await inspectFailure(new Response(new ReadableStream({
      pull(controller) {
        pullCount++;
        controller.enqueue(new Uint8Array(
          pullCount === 1 ? ERROR_RESPONSE_BODY_LIMIT : 1,
        ));
      },
      cancel() {
        streamedCancelled = true;
      },
    }), { status: 500 }));

    const payload = { items: ["one", "two"], next_cursor: null };
    const buffered = await bufferResponse(new Response(JSON.stringify(payload), {
      status: 200,
      headers: { "Content-Type": "application/json" },
    }));
    const parsed = await buffered.clone().json();
    const text = await buffered.text();
    const replayPayload = JSON.stringify({ message: "chunked ✓ response" });
    const replayBytes = new TextEncoder().encode(replayPayload);
    const replayed = await bufferResponse(new Response(new ReadableStream({
      start(controller) {
        controller.enqueue(replayBytes.subarray(0, 7));
        controller.enqueue(replayBytes.subarray(7, replayBytes.length - 2));
        controller.enqueue(replayBytes.subarray(replayBytes.length - 2));
        controller.close();
      },
    }), {
      status: 200,
      headers: { "Content-Type": "application/json" },
    }));
    const replayedClone = replayed.clone();
    const [replayedJson, replayedText] = await Promise.all([
      replayed.json(),
      replayedClone.text(),
    ]);
    const legalWorstCasePath = Array.from(
      { length: 16 },
      () => "\u0001".repeat(240),
    ).join("/");
    const legalLargePayload = JSON.stringify({
      items: Array.from({ length: 500 }, (_, index) => ({
        name: legalWorstCasePath,
        mtime: index,
        size: index,
        path_type: "File",
      })),
      next_cursor: null,
    });
    const legalLargeResponse = await bufferResponse(
      new Response(legalLargePayload, {
        status: 200,
        headers: { "Content-Type": "application/json" },
      }),
    );
    const legalLargeResult = await legalLargeResponse.json();

    let successCancelled = false;
    const oversizedSuccess = await inspectFailure(new Response(
      new ReadableStream({
        cancel() {
          successCancelled = true;
        },
      }),
      {
        status: 200,
        headers: {
          "Content-Length": String(SUCCESS_RESPONSE_BODY_LIMIT + 1),
        },
      },
    ));
    return {
      declared,
      declaredCancelled,
      streamed,
      streamedCancelled,
      oversizedSuccess,
      successCancelled,
      parsed,
      text,
      expectedText: JSON.stringify(payload),
      replayedJson,
      replayedText,
      replayPayload,
      legalLargeItemCount: legalLargeResult.items.length,
      legalLargeNameLength: legalLargeResult.items[0].name.length,
      expectedLegalLargeNameLength: legalWorstCasePath.length,
    };
  });

  for (const result of [
    results.declared,
    results.streamed,
    results.oversizedSuccess,
  ]) {
    expect(result).toEqual({
      accepted: false,
      code: "response_body_too_large",
    });
  }
  expect(results.declaredCancelled).toBe(true);
  expect(results.streamedCancelled).toBe(true);
  expect(results.successCancelled).toBe(true);
  expect(results.parsed).toEqual({ items: ["one", "two"], next_cursor: null });
  expect(results.text).toBe(results.expectedText);
  expect(results.replayedJson).toEqual({ message: "chunked ✓ response" });
  expect(results.replayedText).toBe(results.replayPayload);
  expect(results.legalLargeItemCount).toBe(500);
  expect(results.legalLargeNameLength).toBe(
    results.expectedLegalLargeNameLength,
  );
});

test("立即新建空文件遇到 429 not-started 不查询也不重放 PUT", async ({
  appPage: page,
}) => {
  let putCount = 0;
  let headCount = 0;
  await page.route("**/newfile", async route => {
    const request = route.request();
    if (request.method() === "HEAD") {
      headCount++;
      await route.abort();
      return;
    }
    putCount++;
    const uploadId = request.headers()["x-dufs-upload-id"];
    await route.fulfill({
      status: 429,
      contentType: "application/problem+json",
      headers: {
        "X-Dufs-Upload-Id": uploadId,
        "X-Dufs-Upload-Length": "0",
        "X-Dufs-Operation-State": "not-started",
      },
      body: JSON.stringify({
        type: "urn:dufs:problem:upload_concurrency_limit",
        title: "Too Many Requests",
        status: 429,
        detail: "Too many concurrent uploads",
        code: "upload_concurrency_limit",
        recovery: "retry",
        upload_id: uploadId,
        upload_state: "not-started",
        upload_length: 0,
      }),
    });
  });
  await page.getByRole("button", { name: "New empty file" }).click();
  const errorDialog = actionDialog(page, "Create file failed");
  await expect(errorDialog).toContainText("Too many concurrent uploads");
  expect(putCount).toBe(1);
  expect(headCount).toBe(0);
  await expect(page.getByRole("button", {
    name: "Refresh",
    exact: true,
  })).toHaveCount(0);
  await errorDialog.getByRole("button", { name: "Close" }).click();
});

test("立即新建空文件异常 2xx 只用同一 upload ID 做一次 HEAD 确认", async ({
  appPage: page,
}) => {
  let putCount = 0;
  let headCount = 0;
  let uploadId = "";
  let documentRequests = 0;
  page.on("request", request => {
    if (request.resourceType() === "document") documentRequests++;
  });
  await page.route(
    "**/__dufs__/api/upload/preflight",
    fulfillCreatedItemPreflight,
  );
  await page.route("**/newfile", async route => {
    const request = route.request();
    if (request.method() === "HEAD") {
      headCount++;
      expect(request.headers()["x-dufs-upload-id"]).toBe(uploadId);
      await route.fulfill({
        status: 200,
        headers: {
          "X-Dufs-Upload-Id": uploadId,
          "X-Dufs-Upload-Length": "0",
          "X-Dufs-Upload-Offset": "0",
          "X-Dufs-Operation-State": "committed",
        },
        body: "",
      });
      return;
    }
    putCount++;
    uploadId = request.headers()["x-dufs-upload-id"];
    await route.fulfill({
      status: 202,
      headers: {
        "X-Dufs-Upload-Id": uploadId,
        "X-Dufs-Upload-Length": "0",
        "X-Dufs-Upload-Offset": "0",
        "X-Dufs-Operation-State": "committed",
      },
      body: "",
    });
  });
  await page.getByRole("button", { name: "New empty file" }).click();
  await expect.poll(() => headCount).toBe(1);
  expect(putCount).toBe(1);
  expect(headCount).toBe(1);
  expect(documentRequests).toBe(0);
  await expect(inlineNameInput(page)).toHaveValue("newfile");
  await expect(inlineNameInput(page)).toBeFocused();
  await expect(actionDialog(page, "Create file failed")).toBeHidden();
});

test("新建文件夹立即创建 newfolder 并进入原位编辑", async ({
  appPage: page,
}) => {
  const root = currentDirectoryPath(page);
  const directoryUrl = page.url();
  const responsePromise = page.waitForResponse(
    response =>
      response.request().method() === "POST" &&
      new URL(response.url()).pathname.endsWith("/__dufs__/api/mkdir"),
  );
  await page.getByRole("button", { name: "New folder" }).click();
  const response = await responsePromise;
  expect(response.status()).toBe(201);
  expect(response.request().headers()["x-dufs-operation-id"])
    .toMatch(UUID_V4_PATTERN);
  expect(response.request().postDataJSON()).toEqual({
    path: `${root}/newfolder`,
  });
  await expect(page).toHaveURL(directoryUrl);
  await expect(actionDialog(page, "Create folder")).toBeHidden();
  const input = inlineNameInput(page);
  await expect(input).toHaveCount(1);
  await expect(input).toBeFocused();
  await expect(input).toHaveValue("newfolder");
  await expectSelection(input, "newfolder".length, "newfolder".length);
  const renamePromise = page.waitForResponse(
    candidate =>
      candidate.request().method() === "POST" &&
      new URL(candidate.url()).pathname.endsWith("/__dufs__/api/rename"),
  );
  await input.fill("created-by-browser");
  await input.press("Enter");
  const renameResponse = await renamePromise;
  expect(renameResponse.status()).toBe(204);
  expect(renameResponse.request().postDataJSON()).toEqual({
    source: `${root}/newfolder`,
    name: "created-by-browser",
    source_revision: expect.stringMatching(/^[0-9a-f]{64}$/),
    overwrite: false,
  });
  await expect(input).toHaveCount(0);
  await expect(rowByName(page, "created-by-browser")).toBeVisible();
});

test("新建空文件立即 PUT newfile 并进入原位编辑", async ({
  appPage: page,
}) => {
  const responsePromise = page.waitForResponse(
    response =>
      response.request().method() === "PUT" &&
      new URL(response.url()).pathname.endsWith("/newfile"),
  );
  await page.getByRole("button", { name: "New empty file" }).click();
  const response = await responsePromise;
  expect(response.status()).toBe(201);
  expect(response.request().headers()["x-dufs-upload-length"]).toBe("0");
  expect(response.request().headers()["x-dufs-upload-id"])
    .toMatch(UUID_V4_PATTERN);
  expect(response.request().headers()["x-dufs-upload-overwrite"]).toBe("false");
  await expect(actionDialog(page, "Create empty file")).toBeHidden();
  const input = inlineNameInput(page);
  await expect(input).toHaveCount(1);
  await expect(input).toBeFocused();
  await expect(input).toHaveValue("newfile");
  await expectSelection(input, "newfile".length, "newfile".length);
  await input.press("Escape");
  await expect(input).toHaveCount(0);
  await expect(rowByName(page, "newfile")).toBeVisible();
});

test("新建完成时清除并发启动的旧行编辑器", async ({ appPage: page }) => {
  let releasePreflight;
  let markPreflightStarted;
  const preflightGate = new Promise(resolve => {
    releasePreflight = resolve;
  });
  const preflightStarted = new Promise(resolve => {
    markPreflightStarted = resolve;
  });
  await page.route("**/__dufs__/api/upload/preflight", async route => {
    markPreflightStarted();
    await preflightGate;
    await fulfillCreatedItemPreflight(route);
  });

  await page.getByRole("button", { name: "New empty file" }).click();
  await preflightStarted;
  await rowByName(page, "rename-me.txt")
    .getByRole("button", { name: "Rename rename-me.txt" })
    .click();
  await expect(inlineNameInput(page)).toHaveValue("rename-me.txt");

  releasePreflight();
  await expect(inlineNameInput(page)).toHaveCount(1);
  await expect(inlineNameInput(page)).toHaveValue("newfile");
  await expect(inlineNameInput(page)).toBeFocused();
  await expect(rowByName(page, "rename-me.txt")).toBeVisible();
  await expect(page.locator(".paths-table tbody tr.is-renaming")).toHaveCount(1);
});

for (const [label, createdName] of [
  ["New folder", "newfolder"],
  ["New empty file", "newfile"],
]) {
  test(`${label} 提交后无需等待 revision 查询就使列表失效`, async ({
    appPage: page,
  }) => {
    let releasePreflight;
    let markPreflightStarted;
    const preflightGate = new Promise(resolve => {
      releasePreflight = resolve;
    });
    const preflightStarted = new Promise(resolve => {
      markPreflightStarted = resolve;
    });
    await page.route("**/__dufs__/api/upload/preflight", async route => {
      markPreflightStarted();
      await preflightGate;
      await fulfillCreatedItemPreflight(route);
    });

    await page.getByRole("button", { name: label, exact: true }).click();
    await preflightStarted;

    const refresh = page.getByRole("button", { name: "Refresh", exact: true });
    await expect(refresh).toBeVisible();
    await expect(page.locator(".list-status")).toContainText(
      "Folder contents changed",
    );

    releasePreflight();
    await expect(inlineNameInput(page)).toHaveValue(createdName);
  });
}

test("文件夹默认名仅在可信冲突后递增且每个候选使用新操作 ID", async ({
  appPage: page,
}) => {
  await seedFolder(page, "newfolder");
  await seedFolder(page, "newfolder (2)");
  const requests = [];
  page.on("request", request => {
    if (
      request.method() === "POST" &&
      new URL(request.url()).pathname.endsWith("/__dufs__/api/mkdir")
    ) {
      requests.push({
        body: request.postDataJSON(),
        operationId: request.headers()["x-dufs-operation-id"],
      });
    }
  });

  await page.getByRole("button", { name: "New folder" }).click();
  const input = inlineNameInput(page);
  await expect(input).toHaveValue("newfolder (3)");
  await expect(input).toBeFocused();

  const root = currentDirectoryPath(page);
  expect(requests.map(request => request.body)).toEqual([
    { path: `${root}/newfolder` },
    { path: `${root}/newfolder (2)` },
    { path: `${root}/newfolder (3)` },
  ]);
  expect(requests.map(request => request.operationId))
    .toEqual(requests.map(request => expect.stringMatching(UUID_V4_PATTERN)));
  expect(new Set(requests.map(request => request.operationId)).size).toBe(3);
});

test("job 中错误状态的 path_exists 不得递增文件夹默认名", async ({
  appPage: page,
}) => {
  const requests = [];
  let statusQueries = 0;
  await page.route("**/__dufs__/api/mkdir", route => {
    requests.push(route.request().postDataJSON());
    if (requests.length > 1) {
      return fulfillTrackedFailure(
        route,
        500,
        "unexpected_second_attempt",
        "A second candidate must not be attempted",
      );
    }
    const operationId = route.request().headers()["x-dufs-operation-id"];
    return route.fulfill({
      status: 500,
      contentType: "application/problem+json",
      headers: {
        "X-Dufs-Operation-Id": operationId,
        "X-Dufs-Operation-State": "unknown",
      },
      body: JSON.stringify({
        type: "urn:dufs:problem:outcome_uncertain",
        title: "Internal server error",
        status: 500,
        code: "outcome_uncertain",
        detail: "The create result must be reconciled",
        recovery: "query_job",
      }),
    });
  });
  await page.route("**/__dufs__/api/jobs/*", route => {
    statusQueries++;
    const operationId = new URL(route.request().url()).pathname.split("/").pop();
    return route.fulfill({
      status: 200,
      contentType: "application/json",
      headers: {
        "X-Dufs-Operation-Id": operationId,
        "X-Dufs-Operation-State": "failed",
      },
      body: JSON.stringify({
        job_id: operationId,
        state: "failed",
        http_status: 500,
        code: "path_exists",
        detail: "The server failed without proving a name conflict",
      }),
    });
  });

  await page.getByRole("button", { name: "New folder" }).click();
  await expect(actionDialog(page, "Create folder failed")).toBeVisible();
  expect(requests).toEqual([
    { path: `${currentDirectoryPath(page)}/newfolder` },
  ]);
  expect(statusQueries).toBe(1);
  await expect(inlineNameInput(page)).toHaveCount(0);
});

test("两种新建共享单一 pending，快速连点不会并发创建或覆盖编辑器", async ({
  appPage: page,
}) => {
  let releaseFolder;
  let markFolderStarted;
  let filePuts = 0;
  const folderGate = new Promise(resolve => {
    releaseFolder = resolve;
  });
  const folderStarted = new Promise(resolve => {
    markFolderStarted = resolve;
  });
  await page.route("**/__dufs__/api/mkdir", async route => {
    markFolderStarted();
    await folderGate;
    await route.continue();
  });
  page.on("request", request => {
    if (request.method() === "PUT") filePuts++;
  });

  await page.getByRole("button", { name: "New folder" }).click();
  await folderStarted;
  await expect(page.locator(".operation-status")).toHaveText(
    "Creating a new folder…",
  );
  await page.getByRole("button", { name: "New empty file" }).click();
  await page.evaluate(() => new Promise(resolve => {
    requestAnimationFrame(() => requestAnimationFrame(resolve));
  }));
  expect(filePuts).toBe(0);
  await expect(page.locator(".operation-status")).toHaveText(
    "Creating a new folder…",
  );

  releaseFolder();
  await expect(inlineNameInput(page)).toHaveValue("newfolder");
  await expect(inlineNameInput(page)).toHaveCount(1);
  await expect(inlineNameInput(page)).toBeFocused();
  expect(filePuts).toBe(0);
});

test("空文件普通现存目标在 not-started 冲突后直接递增且不 discard", async ({
  appPage: page,
}) => {
  await seedEmptyFile(page, "newfile");
  await seedEmptyFile(page, "newfile (2)");
  const puts = [];
  const discards = [];
  page.on("request", request => {
    const url = new URL(request.url());
    if (request.method() === "PUT") {
      puts.push({
        path: decodeURIComponent(url.pathname),
        uploadId: request.headers()["x-dufs-upload-id"],
        overwrite: request.headers()["x-dufs-upload-overwrite"],
      });
    } else if (
      request.method() === "POST" &&
      url.pathname.endsWith("/__dufs__/api/upload/discard")
    ) {
      discards.push(request.postDataJSON());
    }
  });

  await page.getByRole("button", { name: "New empty file" }).click();
  await expect(inlineNameInput(page)).toHaveValue("newfile (3)");

  const root = currentDirectoryPath(page);
  expect(puts.map(request => request.path)).toEqual([
    `${root}/newfile`,
    `${root}/newfile (2)`,
    `${root}/newfile (3)`,
  ]);
  expect(puts.every(request => request.overwrite === "false")).toBe(true);
  expect(puts.map(request => request.uploadId))
    .toEqual(puts.map(request => expect.stringMatching(UUID_V4_PATTERN)));
  expect(new Set(puts.map(request => request.uploadId)).size).toBe(3);
  expect(discards).toEqual([]);
});

test("空文件 awaiting-confirmation 冲突先 discard 成功再递增", async ({
  appPage: page,
}) => {
  const puts = [];
  const discards = [];
  await page.route(
    "**/__dufs__/api/upload/preflight",
    fulfillCreatedItemPreflight,
  );
  await page.route("**/newfile*", route => {
    const request = route.request();
    if (request.method() !== "PUT") return route.continue();
    const uploadId = request.headers()["x-dufs-upload-id"];
    puts.push({
      path: decodeURIComponent(new URL(request.url()).pathname),
      uploadId,
    });
    if (puts.length === 1) {
      return route.fulfill({
        status: 409,
        contentType: "application/problem+json",
        headers: {
          "X-Dufs-Upload-Id": uploadId,
          "X-Dufs-Upload-Length": "0",
          "X-Dufs-Upload-Offset": "0",
          "X-Dufs-Operation-State": "awaiting-confirmation",
          "X-Dufs-Target-Revision": "a".repeat(64),
          "X-Dufs-Target-Replaceable": "true",
        },
        body: JSON.stringify({
          type: "urn:dufs:problem:destination_exists",
          title: "Conflict",
          status: 409,
          code: "destination_exists",
          detail: "Destination exists after staging",
          recovery: "refresh_target",
        }),
      });
    }
    return route.fulfill({
      status: 201,
      headers: {
        "X-Dufs-Upload-Id": uploadId,
        "X-Dufs-Upload-Length": "0",
        "X-Dufs-Upload-Offset": "0",
        "X-Dufs-Operation-State": "committed",
      },
      body: "",
    });
  });
  await page.route("**/__dufs__/api/upload/discard", route => {
    const payload = route.request().postDataJSON();
    discards.push(payload);
    return route.fulfill({
      status: 204,
      headers: discardProtocolHeaders(payload.upload_id, 0),
      body: "",
    });
  });

  await page.getByRole("button", { name: "New empty file" }).click();
  await expect(inlineNameInput(page)).toHaveValue("newfile (2)");
  const root = currentDirectoryPath(page);
  expect(puts.map(request => request.path)).toEqual([
    `${root}/newfile`,
    `${root}/newfile (2)`,
  ]);
  expect(puts[0].uploadId).toMatch(UUID_V4_PATTERN);
  expect(puts[1].uploadId).toMatch(UUID_V4_PATTERN);
  expect(puts[1].uploadId).not.toBe(puts[0].uploadId);
  expect(discards).toEqual([{
    path: `${root}/newfile`,
    upload_id: puts[0].uploadId,
  }]);
});

test("空文件 rejected 冲突无需 discard 即可用新 ID 尝试下一候选", async ({
  appPage: page,
}) => {
  const puts = [];
  let discards = 0;
  await page.route(
    "**/__dufs__/api/upload/preflight",
    fulfillCreatedItemPreflight,
  );
  await page.route("**/newfile*", route => {
    const request = route.request();
    if (request.method() !== "PUT") return route.continue();
    const uploadId = request.headers()["x-dufs-upload-id"];
    puts.push({
      path: decodeURIComponent(new URL(request.url()).pathname),
      uploadId,
    });
    if (puts.length === 1) {
      return route.fulfill({
        status: 409,
        contentType: "application/problem+json",
        headers: {
          "X-Dufs-Upload-Id": uploadId,
          "X-Dufs-Upload-Length": "0",
          "X-Dufs-Operation-State": "rejected",
        },
        body: JSON.stringify({
          type: "urn:dufs:problem:destination_exists",
          title: "Conflict",
          status: 409,
          code: "destination_exists",
          detail: "Destination exists",
        }),
      });
    }
    return route.fulfill({
      status: 201,
      headers: {
        "X-Dufs-Upload-Id": uploadId,
        "X-Dufs-Upload-Length": "0",
        "X-Dufs-Upload-Offset": "0",
        "X-Dufs-Operation-State": "committed",
      },
      body: "",
    });
  });
  await page.route("**/__dufs__/api/upload/discard", route => {
    discards++;
    const payload = route.request().postDataJSON();
    return route.fulfill({
      status: 204,
      headers: discardProtocolHeaders(payload.upload_id, 0),
      body: "",
    });
  });

  await page.getByRole("button", { name: "New empty file" }).click();
  await expect(inlineNameInput(page)).toHaveValue("newfile (2)");
  expect(puts.map(request => request.path)).toEqual([
    `${currentDirectoryPath(page)}/newfile`,
    `${currentDirectoryPath(page)}/newfile (2)`,
  ]);
  expect(puts[0].uploadId).toMatch(UUID_V4_PATTERN);
  expect(puts[1].uploadId).toMatch(UUID_V4_PATTERN);
  expect(puts[1].uploadId).not.toBe(puts[0].uploadId);
  expect(discards).toBe(0);
});

test("HTTP 500 的 destination_exists 不得递增空文件默认名", async ({
  appPage: page,
}) => {
  const puts = [];
  let discards = 0;
  await page.route("**/newfile*", route => {
    const request = route.request();
    if (request.method() !== "PUT") return route.continue();
    const uploadId = request.headers()["x-dufs-upload-id"];
    puts.push(decodeURIComponent(new URL(request.url()).pathname));
    const firstAttempt = puts.length === 1;
    const status = firstAttempt ? 500 : 507;
    const code = firstAttempt
      ? "destination_exists"
      : "unexpected_second_attempt";
    return route.fulfill({
      status,
      contentType: "application/problem+json",
      headers: {
        "X-Dufs-Upload-Id": uploadId,
        "X-Dufs-Upload-Length": "0",
        "X-Dufs-Operation-State": "rejected",
      },
      body: JSON.stringify({
        type: `urn:dufs:problem:${code}`,
        title: "Upload rejected",
        status,
        code,
        detail: firstAttempt
          ? "The server failed without proving a destination conflict"
          : "A second candidate must not be attempted",
      }),
    });
  });
  await page.route("**/__dufs__/api/upload/discard", route => {
    discards++;
    return route.fulfill({ status: 500, body: "unexpected discard" });
  });

  await page.getByRole("button", { name: "New empty file" }).click();
  await expect(actionDialog(page, "Create file failed")).toBeVisible();
  expect(puts).toEqual([`${currentDirectoryPath(page)}/newfile`]);
  expect(discards).toBe(0);
  await expect(inlineNameInput(page)).toHaveCount(0);
});

test("新建文件夹 outcome unknown 时不尝试下一个候选名", async ({
  appPage: page,
}) => {
  const requests = [];
  let statusQueries = 0;
  await page.route("**/__dufs__/api/mkdir", route => {
    const operationId = route.request().headers()["x-dufs-operation-id"];
    requests.push(route.request().postDataJSON());
    return route.fulfill({
      status: 409,
      contentType: "application/problem+json",
      headers: {
        "X-Dufs-Operation-Id": operationId,
        "X-Dufs-Operation-State": "unknown",
      },
      body: JSON.stringify({
        type: "urn:dufs:problem:path_exists",
        title: "Conflict",
        status: 409,
        code: "path_exists",
        detail: "The create result is unknown",
        recovery: "query_job",
      }),
    });
  });
  await page.route("**/__dufs__/api/jobs/*", route => {
    statusQueries++;
    const operationId = new URL(route.request().url()).pathname.split("/").pop();
    return route.fulfill({
      status: 200,
      contentType: "application/json",
      headers: {
        "X-Dufs-Operation-Id": operationId,
        "X-Dufs-Operation-State": "unknown",
      },
      body: JSON.stringify({
        job_id: operationId,
        state: "unknown",
        http_status: 409,
        code: "outcome_uncertain",
        detail: "The create result is unknown",
      }),
    });
  });

  await page.getByRole("button", { name: "New folder" }).click();
  await expect(actionDialog(page, "Create folder failed")).toBeVisible();
  expect(requests).toEqual([
    { path: `${currentDirectoryPath(page)}/newfolder` },
  ]);
  expect(statusQueries).toBe(1);
  await expect(inlineNameInput(page)).toHaveCount(0);
  await expect(page.getByRole("button", {
    name: "Refresh",
    exact: true,
  })).toBeVisible();
});

test("discard 结果不可信时不再 PUT 下一个空文件候选", async ({
  appPage: page,
}) => {
  const puts = [];
  let discards = 0;
  await page.route("**/newfile", route => {
    const request = route.request();
    const uploadId = request.headers()["x-dufs-upload-id"];
    puts.push({ path: new URL(request.url()).pathname, uploadId });
    return route.fulfill({
      status: 409,
      contentType: "application/problem+json",
      headers: {
        "X-Dufs-Upload-Id": uploadId,
        "X-Dufs-Upload-Length": "0",
        "X-Dufs-Upload-Offset": "0",
        "X-Dufs-Operation-State": "awaiting-confirmation",
        "X-Dufs-Target-Revision": "b".repeat(64),
        "X-Dufs-Target-Replaceable": "true",
      },
      body: JSON.stringify({
        type: "urn:dufs:problem:destination_exists",
        title: "Conflict",
        status: 409,
        code: "destination_exists",
        detail: "Destination exists",
        recovery: "refresh_target",
      }),
    });
  });
  await page.route("**/__dufs__/api/upload/discard", route => {
    discards++;
    return route.fulfill({ status: 502, body: "discard outcome unknown" });
  });

  await page.getByRole("button", { name: "New empty file" }).click();
  await expect(actionDialog(page, "Create file failed")).toBeVisible();
  expect(puts).toHaveLength(1);
  expect(discards).toBe(1);
  await expect(inlineNameInput(page)).toHaveCount(0);
  await expect(page.getByRole("button", {
    name: "Refresh",
    exact: true,
  })).toBeVisible();
  await expect(page.locator(".list-status")).toContainText(
    "may have changed",
  );
});

test("使用独立重命名接口并刷新当前目录", async ({ appPage: page }) => {
  const source = currentLogicalChild(page, "rename-me.txt");
  const directoryUrl = page.url();
  const responsePromise = page.waitForResponse(
    response =>
      response.request().method() === "POST" &&
      new URL(response.url()).pathname.endsWith("/__dufs__/api/rename"),
  );
  await rowByName(page, "rename-me.txt")
    .getByRole("button", { name: "Rename rename-me.txt" })
    .click();
  const input = inlineNameInput(page);
  await expect(input).toBeFocused();
  await expect(input).toHaveValue("rename-me.txt");
  await expect(input.locator("xpath=ancestor::td")).toHaveClass(/\bcell-name\b/);
  await expect(input.locator("xpath=ancestor::tr")).toHaveClass(/\bis-renaming\b/);
  await expect(actionDialog(page, "Rename item")).toBeHidden();
  await expectSelection(input, "rename-me".length, "rename-me".length);
  await input.fill("renamed-by-browser.txt");
  await input.press("Enter");
  const response = await responsePromise;
  expect(response.status()).toBe(204);
  expect(response.request().headers()["x-dufs-operation-id"]).toMatch(
    /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/,
  );
  expect(response.request().postDataJSON()).toEqual({
    source,
    name: "renamed-by-browser.txt",
    source_revision: expect.stringMatching(/^[0-9a-f]{64}$/),
    overwrite: false,
  });
  await expect(page).toHaveURL(directoryUrl);
  await expect(
    page.getByRole("link", { name: "renamed-by-browser.txt", exact: true }),
  ).toBeVisible();
  await expect(rowByName(page, "rename-me.txt")).toHaveCount(0);
});

test("慢重命名进入 busy 时同步 blur 不会重复提交", async ({
  appPage: page,
}) => {
  let releaseRename;
  let markRenameStarted;
  let renameRequests = 0;
  const renameGate = new Promise(resolve => {
    releaseRename = resolve;
  });
  const renameStarted = new Promise(resolve => {
    markRenameStarted = resolve;
  });
  await page.route("**/__dufs__/api/rename", async route => {
    renameRequests++;
    markRenameStarted();
    await renameGate;
    await route.continue();
  });

  await rowByName(page, "rename-me.txt")
    .getByRole("button", { name: "Rename rename-me.txt" })
    .click();
  const input = inlineNameInput(page);
  await input.fill("slow-renamed.txt");
  await input.press("Enter");
  await renameStarted;
  await expect(input).toBeDisabled();
  await expect(input).toHaveAttribute("aria-busy", "true");
  await page.evaluate(() => new Promise(resolve => {
    requestAnimationFrame(() => requestAnimationFrame(resolve));
  }));
  expect(renameRequests).toBe(1);

  releaseRename();
  const renamedLink = page.getByRole("link", {
    name: "slow-renamed.txt",
    exact: true,
  });
  await expect(renamedLink).toBeVisible();
  await expect(input).toHaveCount(0);
  await expect(renamedLink).toBeFocused();
  expect(renameRequests).toBe(1);
});

test("行内重命名支持 Tab、失焦提交和 Escape 取消且全局只有一个编辑器", async ({
  appPage: page,
}) => {
  const requests = [];
  page.on("request", request => {
    if (
      request.method() === "POST" &&
      new URL(request.url()).pathname.endsWith("/__dufs__/api/rename")
    ) {
      requests.push(request.postDataJSON());
    }
  });

  const firstRename = rowByName(page, "rename-me.txt").getByRole("button", {
    name: "Rename rename-me.txt",
  });
  await firstRename.click();
  const firstInput = inlineNameInput(page);
  await expect(firstInput).toBeFocused();
  await expectSelection(firstInput, "rename-me".length, "rename-me".length);

  const secondRename = rowByName(page, "download-me.txt").getByRole(
    "button",
    { name: "Rename download-me.txt" },
  );
  await secondRename.click();
  await expect(inlineNameInput(page)).toHaveCount(1);
  await expect(inlineNameInput(page)).toHaveValue("download-me.txt");
  await expect(inlineNameInput(page)).toBeFocused();
  expect(requests).toHaveLength(0);

  await inlineNameInput(page).fill("tab-renamed.txt");
  await inlineNameInput(page).press("Tab");
  await expect(rowByName(page, "tab-renamed.txt")).toBeVisible();
  await expect(rowByName(page, "tab-renamed.txt").getByRole("button", {
    name: "Move tab-renamed.txt",
  })).toBeFocused();
  await expect(inlineNameInput(page)).toHaveCount(0);

  const escapeRename = rowByName(page, "rename-me.txt").getByRole("button", {
    name: "Rename rename-me.txt",
  });
  await escapeRename.click();
  await inlineNameInput(page).fill("must-not-commit.txt");
  await inlineNameInput(page).press("Escape");
  await expect(rowByName(page, "rename-me.txt")).toBeVisible();
  await expect(escapeRename).toBeFocused();

  await escapeRename.click();
  await inlineNameInput(page).fill("blur-renamed.txt");
  await page.getByRole("button", { name: "New folder" }).focus();
  await expect(rowByName(page, "blur-renamed.txt")).toBeVisible();
  expect(requests.map(request => request.name)).toEqual([
    "tab-renamed.txt",
    "blur-renamed.txt",
  ]);
});

test("进入原位编辑时不自动选中文本并将光标放在名称编辑位置", async ({
  appPage: page,
}) => {
  await rowByName(page, "existing-folder")
    .getByRole("button", { name: "Rename existing-folder" })
    .click();
  await expectSelection(
    inlineNameInput(page),
    "existing-folder".length,
    "existing-folder".length,
  );
  await inlineNameInput(page).press("Escape");

  await rowByName(page, "special & # + 中文.txt")
    .getByRole("button", { name: "Rename special & # + 中文.txt" })
    .click();
  await expectSelection(
    inlineNameInput(page),
    "special & # + 中文".length,
    "special & # + 中文".length,
  );
  await inlineNameInput(page).press("Escape");
});

test("IME 组合输入中的 Escape 不取消编辑，普通 Escape 才取消", async ({
  appPage: page,
}) => {
  let renameRequests = 0;
  page.on("request", request => {
    if (
      request.method() === "POST" &&
      new URL(request.url()).pathname.endsWith("/__dufs__/api/rename")
    ) {
      renameRequests++;
    }
  });
  await rowByName(page, "rename-me.txt")
    .getByRole("button", { name: "Rename rename-me.txt" })
    .click();
  const input = inlineNameInput(page);
  await input.fill("组合输入.txt");
  await input.dispatchEvent("keydown", {
    key: "Escape",
    bubbles: true,
    isComposing: true,
  });
  await expect(input).toHaveValue("组合输入.txt");
  await expect(input).toBeFocused();
  expect(renameRequests).toBe(0);

  await input.press("Escape");
  await expect(input).toHaveCount(0);
  await expect(rowByName(page, "rename-me.txt")).toBeVisible();
  expect(renameRequests).toBe(0);
});

test("搜索和排序状态下仍先原位改名，结束后按过滤器刷新", async ({
  appPage: page,
}) => {
  const url = new URL(page.url());
  url.searchParams.set("q", "does-not-match-default-name");
  url.searchParams.set("sort", "size");
  url.searchParams.set("order", "desc");
  await page.goto(url.href);
  const trigger = page.getByRole("button", { name: "New folder" });
  await trigger.click();

  const input = inlineNameInput(page);
  await expect(input).toHaveValue("newfolder");
  await expect(input).toBeFocused();
  expect(new URL(page.url()).searchParams.get("q"))
    .toBe("does-not-match-default-name");
  expect(new URL(page.url()).searchParams.get("sort")).toBe("size");
  expect(new URL(page.url()).searchParams.get("order")).toBe("desc");

  await input.press("Escape");
  await expect(input).toHaveCount(0);
  await expect(page.getByRole("link", {
    name: "newfolder",
    exact: true,
  })).toHaveCount(0);
  await expect(page.locator(".list-status")).toContainText(/created.*hidden/i);
  await expect(trigger).toBeFocused();
});

test("使用独立移动接口保留名称并进入目标目录", async ({
  appPage: page,
}) => {
  const source = currentLogicalChild(page, "rename-me.txt");
  const directory = currentLogicalChild(page, "existing-folder");
  const responsePromise = page.waitForResponse(
    response =>
      response.request().method() === "POST" &&
      new URL(response.url()).pathname.endsWith("/__dufs__/api/move"),
  );
  await rowByName(page, "rename-me.txt")
    .getByRole("button", { name: "Move rename-me.txt" })
    .click();
  await submitActionDialog(page, {
    title: "Move item",
    label: "Destination folder",
    value: directory,
    confirmText: "Move",
  });
  const response = await responsePromise;
  expect(response.status()).toBe(204);
  expect(response.request().headers()["x-dufs-operation-id"]).toMatch(
    /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/,
  );
  expect(response.request().postDataJSON()).toEqual({
    source,
    directory,
    source_revision: expect.stringMatching(/^[0-9a-f]{64}$/),
    overwrite: false,
  });
  await expect.poll(() => currentDirectoryPath(page)).toBe(directory);
  await expect(
    page.getByRole("link", { name: "rename-me.txt", exact: true }),
  ).toBeVisible();
});

test("重命名目标存在时只有确认后才显式覆盖", async ({
  appPage: page,
  context,
}) => {
  const source = currentLogicalChild(page, "overwrite-source.txt");
  const destination = currentLogicalChild(page, "overwrite-target.txt");
  const targetUrl = currentUrl(page, "overwrite-target.txt");
  let targetRevision = "";
  await page.route("**/__dufs__/api/rename", async route => {
    const request = route.request().postDataJSON();
    if (
      request.source === source &&
      request.name === "overwrite-target.txt" &&
      request.overwrite === false
    ) {
      const response = await route.fetch();
      targetRevision = response.headers()["x-dufs-target-revision"] || "";
      await route.fulfill({ response });
      return;
    }
    await route.continue();
  });
  const responses = [];
  page.on("response", response => {
    if (
      response.request().method() === "POST" &&
      new URL(response.url()).pathname.endsWith("/__dufs__/api/rename")
    ) {
      responses.push(response);
    }
  });
  await rowByName(page, "overwrite-source.txt")
    .getByRole("button", { name: "Rename overwrite-source.txt" })
    .click();
  const input = inlineNameInput(page);
  await input.fill("overwrite-target.txt");
  await input.press("Enter");
  const overwriteDialog = actionDialog(page, "Overwrite destination?");
  await expect(overwriteDialog).toContainText(
    `Replace "${destination}" with "${source}"?`,
  );
  await overwriteDialog.getByRole("button", { name: "Overwrite" }).click();
  await expect.poll(() => responses.length).toBe(2);
  expect(responses.map(response => response.status())).toEqual([409, 204]);
  expect(targetRevision).toMatch(/^[0-9a-f]{64}$/);
  const sourceRevision = responses[0].request().postDataJSON().source_revision;
  expect(sourceRevision).toMatch(/^[0-9a-f]{64}$/);
  expect(responses.map(response => response.request().postDataJSON())).toEqual([
    {
      source,
      name: "overwrite-target.txt",
      source_revision: sourceRevision,
      overwrite: false,
    },
    {
      source,
      name: "overwrite-target.txt",
      source_revision: sourceRevision,
      overwrite: true,
      destination_revision: targetRevision,
    },
  ]);
  const contentResponse = await context.request.get(targetUrl);
  expect(contentResponse.status()).toBe(200);
  expect(await contentResponse.text()).toBe("replacement content");
});

test("重命名冲突缺少目标 revision 时拒绝覆盖确认", async ({
  appPage: page,
}) => {
  let renameRequests = 0;
  await page.route("**/__dufs__/api/rename", route => {
    renameRequests++;
    const operationId = route.request().headers()["x-dufs-operation-id"];
    return route.fulfill({
      status: 409,
      contentType: "application/problem+json",
      headers: {
        "X-Dufs-Operation-Id": operationId,
        "X-Dufs-Operation-State": "failed",
      },
      body: JSON.stringify({
        type: "urn:dufs:problem:destination_exists",
        title: "Conflict",
        status: 409,
        detail: "The target name is occupied",
        code: "destination_exists",
      }),
    });
  });

  await rowByName(page, "overwrite-source.txt")
    .getByRole("button", { name: "Rename overwrite-source.txt" })
    .click();
  const input = inlineNameInput(page);
  await input.fill("overwrite-target.txt");
  await input.press("Enter");

  await expect(actionDialog(page, "Rename failed")).toContainText(
    "The target name is occupied",
  );
  await expect(actionDialog(page, "Overwrite destination?")).toBeHidden();
  expect(renameRequests).toBe(1);
});

test("HTTP 500 的 destination_exists 不得授权重命名覆盖", async ({
  appPage: page,
}) => {
  let renameRequests = 0;
  await page.route("**/__dufs__/api/rename", route => {
    renameRequests++;
    const operationId = route.request().headers()["x-dufs-operation-id"];
    return route.fulfill({
      status: 500,
      contentType: "application/problem+json",
      headers: {
        "X-Dufs-Operation-Id": operationId,
        "X-Dufs-Operation-State": "failed",
        "X-Dufs-Target-Revision": "a".repeat(64),
      },
      body: JSON.stringify({
        type: "urn:dufs:problem:destination_exists",
        title: "Internal server error",
        status: 500,
        detail: "The server failed without proving a destination conflict",
        code: "destination_exists",
      }),
    });
  });

  await rowByName(page, "overwrite-source.txt")
    .getByRole("button", { name: "Rename overwrite-source.txt" })
    .click();
  const input = inlineNameInput(page);
  await input.fill("overwrite-target.txt");
  await input.press("Enter");

  await expect(actionDialog(page, "Rename failed")).toContainText(
    "The server failed without proving a destination conflict",
  );
  await expect(actionDialog(page, "Overwrite destination?")).toBeHidden();
  expect(renameRequests).toBe(1);
});

test("重命名源 revision 过期时关闭编辑器并刷新列表", async ({
  appPage: page,
}) => {
  let listRequests = 0;
  let renameRequests = 0;
  page.on("request", request => {
    if (new URL(request.url()).pathname.endsWith("/__dufs__/api/list")) {
      listRequests++;
    }
  });
  await page.route("**/__dufs__/api/rename", route => {
    renameRequests++;
    return fulfillTrackedFailure(
      route,
      412,
      "source_changed",
      "The source changed after the list was loaded",
      { "X-Dufs-Source-Revision": "e".repeat(64) },
    );
  });

  await rowByName(page, "rename-me.txt")
    .getByRole("button", { name: "Rename rename-me.txt" })
    .click();
  const input = inlineNameInput(page);
  await input.fill("stale-source.txt");
  await input.press("Enter");
  const errorDialog = actionDialog(page, "Rename failed");
  await expect(errorDialog).toContainText(
    "The source changed after the list was loaded",
  );
  expect(listRequests).toBe(0);
  await errorDialog.getByRole("button", { name: "Close" }).click();

  await expect.poll(() => listRequests).toBeGreaterThan(0);
  await expect(inlineNameInput(page)).toHaveCount(0);
  await expect(rowByName(page, "rename-me.txt")).toBeVisible();
  expect(renameRequests).toBe(1);
});

test("覆盖确认后目标 revision 过期时不重试并刷新列表", async ({
  appPage: page,
}) => {
  let listRequests = 0;
  let renameRequests = 0;
  page.on("request", request => {
    if (new URL(request.url()).pathname.endsWith("/__dufs__/api/list")) {
      listRequests++;
    }
  });
  await page.route("**/__dufs__/api/rename", route => {
    renameRequests++;
    if (renameRequests === 1) {
      return fulfillTrackedFailure(
        route,
        409,
        "destination_exists",
        "The destination exists",
        { "X-Dufs-Target-Revision": "a".repeat(64) },
      );
    }
    return fulfillTrackedFailure(
      route,
      412,
      "destination_changed",
      "The destination changed after confirmation",
      { "X-Dufs-Target-Revision": "b".repeat(64) },
    );
  });

  await rowByName(page, "overwrite-source.txt")
    .getByRole("button", { name: "Rename overwrite-source.txt" })
    .click();
  const input = inlineNameInput(page);
  await input.fill("overwrite-target.txt");
  await input.press("Enter");
  await actionDialog(page, "Overwrite destination?")
    .getByRole("button", { name: "Overwrite" })
    .click();
  const errorDialog = actionDialog(page, "Rename failed");
  await expect(errorDialog).toContainText(
    "The destination changed after confirmation",
  );
  expect(listRequests).toBe(0);
  await errorDialog.getByRole("button", { name: "Close" }).click();

  await expect.poll(() => listRequests).toBeGreaterThan(0);
  await expect(inlineNameInput(page)).toHaveCount(0);
  await expect(rowByName(page, "overwrite-source.txt")).toBeVisible();
  expect(renameRequests).toBe(2);
});

test("重命名结果 unknown 时不采纳正文冲突码发起覆盖", async ({
  appPage: page,
}) => {
  const source = currentLogicalChild(page, "overwrite-source.txt");
  let renameRequests = 0;
  let statusQueries = 0;
  await page.route("**/__dufs__/api/rename", route => {
    renameRequests++;
    const operationId = route.request().headers()["x-dufs-operation-id"];
    return route.fulfill({
      status: 409,
      contentType: "application/problem+json",
      headers: {
        "X-Dufs-Operation-Id": operationId,
        "X-Dufs-Operation-State": "unknown",
      },
      body: JSON.stringify({
        type: "urn:dufs:problem:destination_exists",
        title: "Conflict",
        status: 409,
        detail: "The target name may be occupied",
        code: "destination_exists",
        recovery: "query_job",
      }),
    });
  });
  await page.route("**/__dufs__/api/jobs/*", route => {
    statusQueries++;
    const operationId = new URL(route.request().url()).pathname.split("/").pop();
    return route.fulfill({
      status: 200,
      contentType: "application/json",
      headers: {
        "X-Dufs-Operation-Id": operationId,
        "X-Dufs-Operation-State": "unknown",
      },
      body: JSON.stringify({
        job_id: operationId,
        state: "unknown",
        http_status: 409,
        code: "outcome_uncertain",
        detail: "The rename outcome is unknown",
      }),
    });
  });

  await rowByName(page, "overwrite-source.txt")
    .getByRole("button", { name: "Rename overwrite-source.txt" })
    .click();
  const input = inlineNameInput(page);
  await input.fill("overwrite-target.txt");
  await input.press("Enter");

  const errorDialog = actionDialog(page, "Rename failed");
  await expect(errorDialog).toContainText(
    "The server could not prove the final operation outcome",
  );
  expect({ renameRequests, statusQueries }).toEqual({
    renameRequests: 1,
    statusQueries: 1,
  });
  await expect(actionDialog(page, "Overwrite destination?")).toBeHidden();
});

test("删除失败时显示错误并保留目录行", async ({ appPage: page }) => {
  await page.route("**/delete-me.txt", async route => {
    if (route.request().method() === "DELETE") {
      await route.fulfill({
        status: 409,
        contentType: "application/problem+json",
        headers: {
          "X-Dufs-Operation-Id":
            route.request().headers()["x-dufs-operation-id"],
          "X-Dufs-Operation-State": "failed",
        },
        body: JSON.stringify({
          type: "urn:dufs:problem:forced_delete_failure",
          title: "Conflict",
          status: 409,
          code: "forced_delete_failure",
          detail: "forced delete failure",
        }),
      });
    } else {
      await route.continue();
    }
  });
  await rowByName(page, "delete-me.txt")
    .getByRole("button", { name: "Delete delete-me.txt" })
    .click();
  await actionDialog(page, "Delete item")
    .getByRole("button", { name: "Delete" })
    .click();
  const errorDialog = actionDialog(page, "Delete failed");
  await expect(errorDialog).toContainText('Unable to delete "delete-me.txt"');
  await expect(errorDialog).toContainText("forced delete failure");
  await expect(rowByName(page, "delete-me.txt")).toBeVisible();
  await expect(page.getByRole("button", {
    name: "Refresh",
    exact: true,
  })).toHaveCount(0);
  await errorDialog.getByRole("button", { name: "Close" }).click();
});

test("删除目标 revision 过期时不删除并刷新列表", async ({
  appPage: page,
}) => {
  let deleteRequests = 0;
  let listRequests = 0;
  page.on("request", request => {
    if (new URL(request.url()).pathname.endsWith("/__dufs__/api/list")) {
      listRequests++;
    }
  });
  await page.route("**/delete-me.txt", route => {
    if (route.request().method() !== "DELETE") return route.continue();
    deleteRequests++;
    return fulfillTrackedFailure(
      route,
      412,
      "delete_target_changed",
      "The delete target changed after the list was loaded",
      { "X-Dufs-Source-Revision": "f".repeat(64) },
    );
  });

  await rowByName(page, "delete-me.txt")
    .getByRole("button", { name: "Delete delete-me.txt" })
    .click();
  await actionDialog(page, "Delete item")
    .getByRole("button", { name: "Delete" })
    .click();
  const errorDialog = actionDialog(page, "Delete failed");
  await expect(errorDialog).toContainText(
    "The delete target changed after the list was loaded",
  );
  expect(listRequests).toBe(0);
  await errorDialog.getByRole("button", { name: "Close" }).click();

  await expect.poll(() => listRequests).toBeGreaterThan(0);
  await expect(rowByName(page, "delete-me.txt")).toBeVisible();
  expect(deleteRequests).toBe(1);
});

test("会话轮换后的 CSRF 响应直接刷新且不查询未登记操作", async ({
  appPage: page,
}) => {
  let statusQueries = 0;
  await page.route("**/__dufs__/api/jobs/*", async route => {
    statusQueries++;
    await route.continue();
  });
  await rotateSession(page);

  await rowByName(page, "delete-me.txt")
    .getByRole("button", { name: "Delete delete-me.txt" })
    .click();
  const reloaded = page.waitForEvent("framenavigated", frame =>
    frame === page.mainFrame()
  );
  await actionDialog(page, "Delete item")
    .getByRole("button", { name: "Delete" })
    .click();
  await reloaded;

  await expect(page.locator(".index-page")).toBeVisible();
  await expect(rowByName(page, "delete-me.txt")).toBeVisible();
  expect(statusQueries).toBe(0);
});

test("取消认证重载后下一次 401 仍会再次请求重载", async ({
  appPage: page,
}) => {
  let releaseUpload;
  let markUploadStarted;
  let mkdirRequests = 0;
  const uploadGate = new Promise(resolve => {
    releaseUpload = resolve;
  });
  const uploadStarted = new Promise(resolve => {
    markUploadStarted = resolve;
  });
  await page.route("**/reload-guard-upload.txt", async route => {
    if (route.request().method() !== "PUT") {
      await route.continue();
      return;
    }
    markUploadStarted();
    await uploadGate;
    await route.fulfill({
      status: 201,
      headers: {
        "X-Dufs-Upload-Id":
          route.request().headers()["x-dufs-upload-id"],
        "X-Dufs-Upload-Length":
          route.request().headers()["x-dufs-upload-length"],
        "X-Dufs-Upload-Offset":
          route.request().headers()["x-dufs-upload-length"],
        "X-Dufs-Operation-State": "committed",
      },
      body: "",
    });
  });
  await page.route("**/__dufs__/api/mkdir", route => {
    mkdirRequests++;
    return route.fulfill({
      status: 401,
      contentType: "application/problem+json",
      body: JSON.stringify({
        type: "urn:dufs:problem:authentication_required",
        title: "Authentication required",
        status: 401,
        code: "authentication_required",
        detail: "Sign in again",
      }),
    });
  });

  const browserDialogTypes = [];
  page.on("dialog", async dialog => {
    browserDialogTypes.push(dialog.type());
    await dialog.dismiss();
  });
  await selectFiles(page, "#file", [{
    name: "reload-guard-upload.txt",
    buffer: Buffer.from("keep the beforeunload guard active"),
  }]);
  await uploadStarted;

  const newFolder = page.getByRole("button", { name: "New folder" });
  await newFolder.click();
  await expect.poll(() => browserDialogTypes).toEqual(["beforeunload"]);
  await expect(newFolder).not.toHaveAttribute("aria-busy", "true");

  await newFolder.click();
  await expect.poll(() => browserDialogTypes).toEqual([
    "beforeunload",
    "beforeunload",
  ]);
  expect(mkdirRequests).toBe(2);

  releaseUpload();
  await expect(page.locator(".upload-status")).toHaveAttribute(
    "aria-label",
    "reload-guard-upload.txt: upload complete",
  );
});

test("提交结果不确定时只查询一次且不盲目重试", async ({ appPage: page }) => {
  await page.clock.install();
  let releaseRequest;
  let operationId = "";
  let deleteRequests = 0;
  let statusQueries = 0;
  const requestGate = new Promise(resolve => {
    releaseRequest = resolve;
  });
  await page.route("**/delete-me.txt", async route => {
    if (route.request().method() !== "DELETE") {
      await route.continue();
      return;
    }
    deleteRequests++;
    operationId = route.request().headers()["x-dufs-operation-id"];
    await requestGate;
    try {
      await route.fulfill({ status: 204, body: "" });
    } catch {
      // The client-side deadline is expected to close the intercepted request.
    }
  });
  await page.route("**/__dufs__/api/jobs/*", async route => {
    statusQueries++;
    expect(new URL(route.request().url()).pathname).toContain(operationId);
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      headers: {
        "X-Dufs-Operation-Id": operationId,
        "X-Dufs-Operation-State": "unknown",
      },
      body: JSON.stringify({
        job_id: operationId,
        state: "unknown",
        http_status: 500,
        code: "outcome_uncertain",
        detail: "Operation outcome is uncertain",
      }),
    });
  });
  await rowByName(page, "delete-me.txt")
    .getByRole("button", { name: "Delete delete-me.txt" })
    .click();
  await actionDialog(page, "Delete item")
    .getByRole("button", { name: "Delete" })
    .click();
  await expect.poll(() => deleteRequests).toBe(1);
  await page.clock.fastForward(30 * 1000 + 1);
  const errorDialog = actionDialog(page, "Delete failed");
  await expect(errorDialog).toContainText("could not prove");
  await expect(page.getByRole("button", {
    name: "Refresh",
    exact: true,
  })).toBeVisible();
  await expect(page.locator(".list-status")).toContainText(
    "Folder contents may have changed",
  );
  expect(deleteRequests).toBe(1);
  expect(statusQueries).toBe(1);
  await expect(rowByName(page, "delete-me.txt")).toBeVisible();
  await errorDialog.getByRole("button", { name: "Close" }).click();
  releaseRequest();
});

test("状态查询确认成功后安全更新页面", async ({ appPage: page }) => {
  await page.clock.install();
  let releaseRequest;
  let operationId = "";
  let backendDeleteStatus = 0;
  const jobResponses = [];
  const requestGate = new Promise(resolve => {
    releaseRequest = resolve;
  });
  await page.route("**/delete-me.txt", async route => {
    if (route.request().method() !== "DELETE") {
      await route.continue();
      return;
    }
    operationId = route.request().headers()["x-dufs-operation-id"];
    const response = await route.fetch();
    backendDeleteStatus = response.status();
    await requestGate;
    try {
      await route.fulfill({ response });
    } catch {
      // The client-side deadline is expected to close the intercepted request.
    }
  });
  page.on("response", response => {
    if (new URL(response.url()).pathname.startsWith("/__dufs__/api/jobs/")) {
      jobResponses.push(response);
    }
  });
  await rowByName(page, "delete-me.txt")
    .getByRole("button", { name: "Delete delete-me.txt" })
    .click();
  await actionDialog(page, "Delete item")
    .getByRole("button", { name: "Delete" })
    .click();
  await expect.poll(() => backendDeleteStatus).toBe(204);
  await page.clock.fastForward(30 * 1000 + 1);
  await expect(rowByName(page, "delete-me.txt")).toHaveCount(0);
  expect(jobResponses).toHaveLength(1);
  expect(new URL(jobResponses[0].url()).pathname).toContain(operationId);
  expect(jobResponses[0].status()).toBe(200);
  releaseRequest();
});

test("作业查询拒绝矛盾头和非法成功状态码", async ({ appPage: page }) => {
  let responseVariant = 0;
  await page.route("**/__dufs__/api/jobs/*", route => {
    const operationId = new URL(route.request().url()).pathname.split("/").pop();
    responseVariant++;
    return route.fulfill({
      status: 200,
      contentType: "application/json",
      headers: {
        "X-Dufs-Operation-Id": operationId,
        "X-Dufs-Operation-State":
          responseVariant === 1 ? "unknown" : "succeeded",
      },
      body: JSON.stringify({
        job_id: operationId,
        state: "succeeded",
        http_status: responseVariant === 1 ? 204 : 500,
      }),
    });
  });

  const results = await page.evaluate(async () => {
    const entrypoint =
      document.querySelector('script[type="module"]').src;
    const { queryJob } = await import(
      new URL("modules/http/client.js", entrypoint).href
    );
    return [
      await queryJob("00000000-0000-4000-8000-000000000010"),
      await queryJob("00000000-0000-4000-8000-000000000011"),
    ];
  });
  for (const result of results) {
    expect(result).toMatchObject({
      kind: "unavailable",
      state: "unknown",
      code: "job_status_unavailable",
      authenticationFailed: false,
    });
  }
});

test("删除成功后更新已加载数量并移动焦点", async ({ appPage: page }) => {
  let listingRequests = 0;
  await page.route("**/__dufs__/api/list?**", route => {
    listingRequests++;
    return listingRequests === 1
      ? fulfillDirectoryChanged(route)
      : route.continue();
  });
  const status = page.locator(".list-status");
  const beforeText = await status.textContent();
  const beforeCount = Number(beforeText.match(/\d+/)?.[0]);
  expect(beforeCount).toBeGreaterThan(0);

  const responsePromise = page.waitForResponse(
    response =>
      response.request().method() === "DELETE" &&
      new URL(response.url()).pathname.endsWith("/delete-me.txt"),
  );
  await rowByName(page, "delete-me.txt")
    .getByRole("button", { name: "Delete delete-me.txt" })
    .click();
  await actionDialog(page, "Delete item")
    .getByRole("button", { name: "Delete" })
    .click();
  const response = await responsePromise;
  expect(response.status()).toBe(204);
  expect(response.request().headers()["if-match"]).toMatch(
    /^"[0-9a-f]{64}"$/,
  );
  await expect(rowByName(page, "delete-me.txt")).toHaveCount(0);
  await expect(status).toHaveText(`All ${beforeCount - 1} items loaded`);
  expect(listingRequests).toBe(2);
  expect(
    await page.evaluate(() => document.activeElement !== document.body),
  ).toBe(true);
});

test("连续目录变化冲突只自动重试一次", async ({ appPage: page }) => {
  let listingRequests = 0;
  await page.route("**/__dufs__/api/list?**", route => {
    listingRequests++;
    return fulfillDirectoryChanged(route);
  });

  await page.reload();
  await expect(page.locator(".list-status")).toHaveText(
    "Unable to load the file list: Directory changed; restart listing",
  );
  await expect(page.getByRole("button", { name: "Retry" })).toBeEnabled();
  expect(listingRequests).toBe(2);
});

test("写入挂起期间新建置顶不会让旧 index 删除错误行", async ({
  appPage: page,
}) => {
  let releaseDelete;
  let deleteTarget = "";
  let renameRequests = 0;
  const deleteGate = new Promise(resolve => {
    releaseDelete = resolve;
  });
  await page.route("**/delete-me.txt", async route => {
    if (route.request().method() !== "DELETE") {
      await route.continue();
      return;
    }
    deleteTarget = decodeURIComponent(new URL(route.request().url()).pathname);
    await deleteGate;
    await route.continue();
  });
  page.on("request", request => {
    if (
      request.method() === "POST" &&
      new URL(request.url()).pathname.endsWith("/__dufs__/api/rename")
    ) {
      renameRequests++;
    }
  });

  const originalDelete = rowByName(page, "delete-me.txt").getByRole(
    "button",
    { name: "Delete delete-me.txt" },
  );
  const originalIndex = await originalDelete.getAttribute("data-index");
  await originalDelete.click();
  await actionDialog(page, "Delete item")
    .getByRole("button", { name: "Delete" })
    .click();
  await expect(page.locator(".operation-status")).toContainText(
    "Deleting delete-me.txt",
  );

  await page.getByRole("button", { name: "New folder" }).click();
  const newItemEditor = inlineNameInput(page);
  await expect(newItemEditor).toHaveValue("newfolder");
  await newItemEditor.fill("unsaved-newfolder");
  const reindexedDelete = rowByName(page, "delete-me.txt").getByRole(
    "button",
    { name: "Delete delete-me.txt" },
  );
  await expect(reindexedDelete).not.toHaveAttribute("data-index", originalIndex);
  await expect(newItemEditor).toBeFocused();
  expect(renameRequests).toBe(0);

  releaseDelete();
  await expect(rowByName(page, "delete-me.txt")).toHaveCount(0);
  await expect(newItemEditor).toHaveValue("unsaved-newfolder");
  await expect(newItemEditor).toBeFocused();
  expect(renameRequests).toBe(0);

  await newItemEditor.press("Escape");
  await expect(newItemEditor).toHaveCount(0);
  await expect(rowByName(page, "newfolder")).toBeVisible();
  await expect(rowByName(page, "unsaved-newfolder")).toHaveCount(0);
  await expect(rowByName(page, "download-me.txt")).toBeVisible();
  expect(deleteTarget).toBe(`${currentDirectoryPath(page)}/delete-me.txt`);
  expect(renameRequests).toBe(0);
});

test("非法行内名称就地标错且不产生重命名请求", async ({ appPage: page }) => {
  let renameRequests = 0;
  page.on("request", request => {
    if (
      request.method() === "POST" &&
      new URL(request.url()).pathname.endsWith("/__dufs__/api/rename")
    ) {
      renameRequests++;
    }
  });
  await rowByName(page, "rename-me.txt")
    .getByRole("button", { name: "Rename rename-me.txt" })
    .click();
  const input = inlineNameInput(page);
  await input.fill("..");
  await input.press("Enter");
  await expect(input).toHaveAttribute("aria-invalid", "true");
  await expect(page.locator(".inline-name-error")).toHaveAttribute(
    "role",
    "alert",
  );
  await expect(page.locator(".inline-name-error")).not.toBeEmpty();
  expect(await input.evaluate(element => {
    const descriptionId = element.getAttribute("aria-describedby");
    return Boolean(
      descriptionId &&
      document.getElementById(descriptionId)?.classList.contains("inline-name-error"),
    );
  })).toBe(true);
  expect(renameRequests).toBe(0);
  await expect(page.getByRole("button", {
    name: "Refresh",
    exact: true,
  })).toHaveCount(0);
  await page.getByRole("button", { name: "New folder" }).focus();
  await expect(inlineNameInput(page)).toHaveCount(0);
  await expect(rowByName(page, "rename-me.txt")).toBeVisible();
  expect(renameRequests).toBe(0);
});

test("长时间操作暴露忙碌状态且保留触发控件焦点能力", async ({
  appPage: page,
}) => {
  let releaseDelete;
  const deleteGate = new Promise(resolve => {
    releaseDelete = resolve;
  });
  await page.route("**/download-me.txt", async route => {
    if (route.request().method() !== "DELETE") {
      await route.continue();
      return;
    }
    await deleteGate;
    await route.fulfill({
      status: 204,
      headers: {
        "X-Dufs-Operation-Id":
          route.request().headers()["x-dufs-operation-id"],
        "X-Dufs-Operation-State": "succeeded",
      },
      body: "",
    });
  });
  const remove = rowByName(page, "download-me.txt").getByRole("button", {
    name: "Delete download-me.txt",
  });
  await remove.click();
  await actionDialog(page, "Delete item")
    .getByRole("button", { name: "Delete" })
    .click();
  await expect(remove).toHaveAttribute("aria-busy", "true");
  await expect(remove).toHaveAttribute("aria-disabled", "true");
  await expect(page.locator(".operation-status")).toHaveText(
    "Deleting download-me.txt…",
  );
  releaseDelete();
  await expect(rowByName(page, "download-me.txt")).toHaveCount(0);
  await expect(page.locator(".operation-status")).toBeHidden();
});
