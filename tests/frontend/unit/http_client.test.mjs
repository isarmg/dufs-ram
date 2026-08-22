import assert from "node:assert/strict";
import test from "node:test";

import {
  RequestError,
  assertDiscardUploadResponse,
} from "../../../assets/modules/http/client.js";

const uploadId = "00000000-0000-4000-8000-000000000001";

test("discard accepts only a bound 204 rejected upload envelope", async () => {
  const validHeaders = {
    "x-dufs-upload-id": uploadId,
    "x-dufs-upload-length": "8",
    "x-dufs-upload-offset": "8",
    "x-dufs-operation-state": "rejected",
  };
  const valid = new Response(null, { status: 204, headers: validHeaders });
  assert.equal(
    await assertDiscardUploadResponse(valid, uploadId, 8),
    valid,
  );

  const invalidResponses = [
    new Response(null, { status: 205, headers: validHeaders }),
    new Response(null, {
      status: 204,
      headers: {
        ...validHeaders,
        "x-dufs-operation-state": "committed",
      },
    }),
    new Response(null, {
      status: 204,
      headers: {
        ...validHeaders,
        "x-dufs-upload-id": "00000000-0000-4000-8000-000000000002",
      },
    }),
    new Response(null, {
      status: 204,
      headers: {
        ...validHeaders,
        "x-dufs-upload-length": "9",
      },
    }),
    new Response(null, {
      status: 204,
      headers: {
        ...validHeaders,
        "x-dufs-upload-offset": "4",
      },
    }),
    new Response(null, {
      status: 204,
      headers: Object.fromEntries(
        Object.entries(validHeaders).filter(([name]) =>
          name !== "x-dufs-upload-offset"
        ),
      ),
    }),
  ];
  for (const response of invalidResponses) {
    await assert.rejects(
      assertDiscardUploadResponse(response, uploadId, 8),
      error => error instanceof RequestError &&
        error.code === "invalid_discard_result" &&
        error.kind === "protocol" &&
        error.outcomeUnknown === true &&
        error.uploadId === uploadId &&
        error.uploadState === "unknown",
    );
  }
});

test("operation errors retain only canonical conditional revisions", async () => {
  const operationId = "00000000-0000-4000-8000-000000000009";
  const sourceRevision = "a".repeat(64);
  const targetRevision = "b".repeat(64);
  const response = new Response(JSON.stringify({
    type: "urn:dufs:problem:destination_exists",
    title: "Conflict",
    status: 409,
    detail: "Destination exists",
    code: "destination_exists",
  }), {
    status: 409,
    headers: {
      "content-type": "application/problem+json",
      "x-dufs-operation-id": operationId,
      "x-dufs-operation-state": "failed",
      "x-dufs-source-revision": sourceRevision,
      "x-dufs-target-revision": targetRevision,
    },
  });
  const { assertResponse } = await import(
    "../../../assets/modules/http/client.js"
  );
  let caught;
  try {
    await assertResponse(response);
  } catch (error) {
    caught = error;
  }
  assert.ok(caught instanceof RequestError);
  assert.equal(caught.sourceRevision, sourceRevision);
  assert.equal(caught.targetRevision, targetRevision);

  const malformed = new Response("Conflict", {
    status: 409,
    headers: {
      "x-dufs-operation-id": operationId,
      "x-dufs-operation-state": "failed",
      "x-dufs-source-revision": sourceRevision.toUpperCase(),
      "x-dufs-target-revision": "opaque",
    },
  });
  try {
    await assertResponse(malformed);
    assert.fail("malformed revision response unexpectedly succeeded");
  } catch (error) {
    assert.ok(error instanceof RequestError);
    assert.equal(error.sourceRevision, "");
    assert.equal(error.targetRevision, "");
  }
});

test("JSON requests validate media type and consume the bounded body once", async () => {
  const previousWindow = globalThis.window;
  const previousFetch = globalThis.fetch;
  globalThis.window = globalThis;
  try {
    const { requestJson, RequestError } = await import(
      "../../../assets/modules/http/client.js"
    );
    globalThis.fetch = async () => new Response('{"ok":true}', {
      headers: { "content-type": "application/problem+json; charset=utf-8" },
    });
    const { response, payload } = await requestJson("https://example.invalid");
    assert.deepEqual(payload, { ok: true });
    assert.equal(response.bodyUsed, true);

    globalThis.fetch = async () => new Response('{"ok":true}', {
      headers: { "content-type": "text/plain" },
    });
    await assert.rejects(
      requestJson("https://example.invalid"),
      error => error instanceof RequestError &&
        error.code === "invalid_json_content_type",
    );
  } finally {
    globalThis.fetch = previousFetch;
    if (previousWindow === undefined) {
      delete globalThis.window;
    } else {
      globalThis.window = previousWindow;
    }
  }
});

test("request cancellation distinguishes pre- and post-dispatch outcomes", async () => {
  const previousWindow = globalThis.window;
  const previousFetch = globalThis.fetch;
  globalThis.window = globalThis;
  let fetchCalls = 0;
  globalThis.fetch = async () => {
    fetchCalls++;
    return new Response(null, { status: 204 });
  };
  try {
    const { requestNoContent, RequestError } = await import(
      "../../../assets/modules/http/client.js"
    );
    const controller = new AbortController();
    controller.abort();
    await assert.rejects(
      requestNoContent(
        "https://example.invalid/mutation",
        { method: "POST", signal: controller.signal },
        {
          outcomeUnknown: true,
          operationId: "00000000-0000-4000-8000-000000000099",
        },
      ),
      error => error instanceof RequestError &&
        error.code === "client_cancelled" &&
        error.kind === "cancelled" &&
        error.outcomeUnknown === false &&
        error.operationState === "",
    );
    assert.equal(fetchCalls, 0);

    const dispatchedController = new AbortController();
    globalThis.fetch = async (_input, init) => {
      fetchCalls++;
      return await new Promise((_resolve, reject) => {
        init.signal.addEventListener("abort", () => {
          reject(new DOMException("Request aborted", "AbortError"));
        }, { once: true });
      });
    };
    const dispatchedRequest = requestNoContent(
      "https://example.invalid/mutation",
      { method: "POST", signal: dispatchedController.signal },
      {
        outcomeUnknown: true,
        operationId: "00000000-0000-4000-8000-000000000100",
      },
    );
    const dispatchedAssertion = assert.rejects(
      dispatchedRequest,
      error => error instanceof RequestError &&
        error.code === "client_cancelled" &&
        error.outcomeUnknown === true &&
        error.operationState === "unknown",
    );
    dispatchedController.abort();
    await dispatchedAssertion;
    assert.equal(fetchCalls, 1);
  } finally {
    globalThis.fetch = previousFetch;
    if (previousWindow === undefined) {
      delete globalThis.window;
    } else {
      globalThis.window = previousWindow;
    }
  }
});

test("operation authentication is classified before its result envelope", async () => {
  const previousWindow = globalThis.window;
  const previousFetch = globalThis.fetch;
  globalThis.window = globalThis;
  try {
    const {
      assertResponse,
      isAuthenticationError,
      requestNoContent,
    } = await import("../../../assets/modules/http/client.js");
    const operationId = "00000000-0000-4000-8000-000000000040";
    const cases = [
      { status: 401, headers: {}, code: "authentication_required" },
      {
        status: 403,
        headers: { "x-dufs-auth-error": "csrf" },
        code: "csrf_failed",
      },
    ];
    for (const entry of cases) {
      globalThis.fetch = async () => new Response(null, {
        status: entry.status,
        headers: entry.headers,
      });
      const response = await requestNoContent(
        "https://example.invalid/mutation",
        { method: "POST" },
        { operationId, outcomeUnknown: true },
      );
      let unauthorizedCalls = 0;
      await assert.rejects(
        assertResponse(response, () => unauthorizedCalls++),
        error => isAuthenticationError(error) && error.code === entry.code,
      );
      assert.equal(unauthorizedCalls, 1);
    }
  } finally {
    globalThis.fetch = previousFetch;
    if (previousWindow === undefined) {
      delete globalThis.window;
    } else {
      globalThis.window = previousWindow;
    }
  }
});

test("mutation reconciliation returns a discriminated result", async () => {
  const {
    runMutationWithReconciliation,
  } = await import("../../../assets/modules/http/client.js");
  const operationId = "00000000-0000-4000-8000-000000000041";
  const successHeaders = {
    "x-dufs-operation-id": operationId,
    "x-dufs-operation-state": "succeeded",
  };
  const succeeded = await runMutationWithReconciliation(
    async () => new Response(null, { status: 204, headers: successHeaders }),
  );
  assert.equal(succeeded.kind, "succeeded");
  assert.equal(succeeded.reconciled, false);

  const failed = await runMutationWithReconciliation(
    async () => new Response("Known failure", {
      status: 409,
      headers: {
        "x-dufs-operation-id": operationId,
        "x-dufs-operation-state": "failed",
      },
    }),
  );
  assert.equal(failed.kind, "failed");
  assert.equal(failed.status, null);
  assert.equal(failed.error.operationState, "failed");
});

test("job queries use the unified route and return discriminated results", async () => {
  const previousWindow = globalThis.window;
  const previousFetch = globalThis.fetch;
  globalThis.window = globalThis;
  try {
    const api = await import("../../../assets/modules/http/client.js");
    assert.equal("queryUnknownOperation" in api, false);
    const {
      queryJob,
      RequestError,
      runMutationWithReconciliation,
    } = api;
    const cases = [
      {
        id: "00000000-0000-4000-8000-000000000042",
        state: "running",
      },
      {
        id: "00000000-0000-4000-8000-000000000043",
        state: "succeeded",
        httpStatus: 204,
      },
      {
        id: "00000000-0000-4000-8000-000000000044",
        state: "failed",
        httpStatus: 409,
      },
      {
        id: "00000000-0000-4000-8000-000000000045",
        state: "unknown",
        httpStatus: 500,
      },
    ];
    const casesById = new Map(cases.map(entry => [entry.id, entry]));
    const requestedPaths = [];
    globalThis.fetch = async input => {
      const path = new URL(String(input), "https://example.invalid").pathname;
      requestedPaths.push(path);
      const id = path.split("/").pop();
      const entry = casesById.get(id);
      assert.ok(entry, `unexpected job ID: ${id}`);
      const payload = {
        job_id: id,
        state: entry.state,
      };
      if (entry.httpStatus !== undefined) {
        payload.http_status = entry.httpStatus;
      }
      return new Response(JSON.stringify(payload), {
        status: 200,
        headers: {
          "content-type": "application/json",
          "x-dufs-operation-id": id,
          "x-dufs-operation-state": entry.state,
        },
      });
    };

    for (const entry of cases) {
      const result = await queryJob(entry.id);
      assert.equal(result.kind, entry.state);
      assert.equal(result.state, entry.state);
      assert.equal(result.jobId, entry.id);
      assert.equal("operationId" in result, false);
    }

    const reconciled = await runMutationWithReconciliation(
      async () => {
        throw new RequestError("unknown", {
          outcomeUnknown: true,
          operationId: cases[1].id,
        });
      },
    );
    assert.equal(reconciled.kind, "succeeded");
    assert.equal(reconciled.reconciled, true);
    assert.ok(requestedPaths.every(path => path.startsWith("/__dufs__/api/jobs/")));
  } finally {
    globalThis.fetch = previousFetch;
    if (previousWindow === undefined) {
      delete globalThis.window;
    } else {
      globalThis.window = previousWindow;
    }
  }
});

test("error payload parsing accepts only canonical Problem Details", async () => {
  const { parseErrorPayload } = await import(
    "../../../assets/modules/http/client.js"
  );
  const operationId = "00000000-0000-4000-8000-000000000010";
  const uploadId = "00000000-0000-4000-8000-000000000011";
  const empty = {
    type: "",
    title: "",
    status: 0,
    detail: "",
    code: "",
    message: "",
    recovery: "",
    retryAfter: null,
    operation: null,
    upload: null,
  };
  const canonical = parseErrorPayload(JSON.stringify({
    type: "urn:dufs:problem:upload_rejected",
    title: "Upload rejected",
    status: 507,
    detail: "There is not enough free space.",
    code: "insufficient_storage",
    recovery: "resume_upload",
    retry_after: 9,
    upload_id: uploadId,
    upload_state: "rejected",
    upload_length: 16,
    upload_offset: 8,
  }), "application/problem+json; charset=utf-8");
  assert.deepEqual(canonical, {
    type: "urn:dufs:problem:upload_rejected",
    title: "Upload rejected",
    status: 507,
    detail: "There is not enough free space.",
    code: "insufficient_storage",
    message: "There is not enough free space.",
    recovery: "resume_upload",
    retryAfter: 9,
    operation: null,
    upload: {
      id: uploadId,
      state: "rejected",
      length: 16,
      offset: 8,
    },
  });

  const operation = parseErrorPayload(JSON.stringify({
    type: "urn:dufs:problem:outcome_uncertain",
    title: "Internal Server Error",
    status: 500,
    detail: "The result is unknown.",
    code: "outcome_uncertain",
    recovery: "query_job",
    operation_id: operationId,
    state: "unknown",
    http_status: 500,
  }), "application/problem+json");
  assert.deepEqual(operation.operation, {
    id: operationId,
    state: "unknown",
    httpStatus: 500,
  });
  assert.equal(operation.recovery, "query_job");

  assert.deepEqual(parseErrorPayload(
    '{"code":"destination_exists","message":"Already there"}',
    "application/json",
  ), empty);
  assert.deepEqual(parseErrorPayload(
    "Destination already exists",
    "text/plain; charset=utf-8",
  ), empty);
  assert.deepEqual(parseErrorPayload(
    "{not-json",
    "application/problem+json",
  ), empty);
  assert.deepEqual(parseErrorPayload(
    '{"message":"Legacy message must be ignored"}',
    "application/problem+json",
  ), empty);

  const nestedAliases = parseErrorPayload(JSON.stringify({
    detail: "Nested aliases are not accepted",
    operation: { id: operationId, state: "failed" },
    upload: { id: uploadId, state: "rejected" },
  }), "application/problem+json");
  assert.equal(nestedAliases.message, "Nested aliases are not accepted");
  assert.equal(nestedAliases.operation, null);
  assert.equal(nestedAliases.upload, null);
});

test("problem status mismatch is a protocol error with HTTP authority", async () => {
  const { assertResponse, RequestError } = await import(
    "../../../assets/modules/http/client.js"
  );
  const headerOperationId = "00000000-0000-4000-8000-000000000020";
  const bodyOperationId = "00000000-0000-4000-8000-000000000021";
  const response = new Response(JSON.stringify({
    type: "https://dufs.example/problems/conflict",
    title: "Conflict",
    status: 400,
    detail: "The destination changed.",
    code: "destination_changed",
    operation_id: bodyOperationId,
    state: "failed",
  }), {
    status: 409,
    headers: {
      "content-type": "application/problem+json",
      "x-dufs-operation-id": headerOperationId,
      "x-dufs-operation-state": "unknown",
    },
  });

  await assert.rejects(
    assertResponse(response),
    error => {
      assert.equal(error instanceof RequestError, true);
      assert.equal(error.status, 409);
      assert.equal(error.problemStatus, 400);
      assert.equal(error.code, "invalid_problem_status");
      assert.equal(error.kind, "protocol");
      assert.equal(error.operationId, headerOperationId);
      assert.equal(error.operationState, "unknown");
      assert.equal(error.operation.id, headerOperationId);
      assert.equal(error.operation.state, "unknown");
      assert.equal(error.outcomeUnknown, true);
      assert.equal(error.detail, "The destination changed.");
      return true;
    },
  );
});

test("unknown outcomes and recovery hints never trigger an automatic retry", async () => {
  const previousWindow = globalThis.window;
  const previousFetch = globalThis.fetch;
  globalThis.window = globalThis;
  let requestCount = 0;
  try {
    const { assertResponse, requestNoContent, RequestError } = await import(
      "../../../assets/modules/http/client.js"
    );
    const operationId = "00000000-0000-4000-8000-000000000030";
    globalThis.fetch = async () => {
      requestCount += 1;
      return new Response(JSON.stringify({
        status: 503,
        detail: "The final outcome is not known.",
        recovery: "retry",
        operation_id: operationId,
        state: "unknown",
      }), {
        status: 503,
        headers: {
          "content-type": "application/problem+json",
          "x-dufs-operation-id": operationId,
          "x-dufs-operation-state": "unknown",
        },
      });
    };

    const response = await requestNoContent("https://example.invalid", {
      method: "POST",
    });
    await assert.rejects(
      assertResponse(response),
      error => error instanceof RequestError &&
        error.outcomeUnknown &&
        error.recovery === "retry",
    );
    assert.equal(requestCount, 1);
  } finally {
    globalThis.fetch = previousFetch;
    if (previousWindow === undefined) {
      delete globalThis.window;
    } else {
      globalThis.window = previousWindow;
    }
  }
});

test("Retry-After header is authoritative over problem metadata", async () => {
  const { assertResponse, RequestError } = await import(
    "../../../assets/modules/http/client.js"
  );
  const response = new Response(JSON.stringify({
    status: 429,
    detail: "Too many concurrent requests",
    code: "request_concurrency_limit",
    recovery: "retry",
    retry_after: 30,
  }), {
    status: 429,
    headers: {
      "content-type": "application/problem+json",
      "retry-after": "2",
    },
  });

  await assert.rejects(
    assertResponse(response),
    error => error instanceof RequestError && error.retryAfter === 2,
  );
});

test("invalid Retry-After header cannot be replaced by body metadata", async () => {
  const { assertResponse, RequestError } = await import(
    "../../../assets/modules/http/client.js"
  );
  const response = new Response(JSON.stringify({
    status: 429,
    detail: "Too many concurrent requests",
    recovery: "retry",
    retry_after: 30,
  }), {
    status: 429,
    headers: {
      "content-type": "application/problem+json",
      "retry-after": "not-a-delta",
    },
  });

  await assert.rejects(
    assertResponse(response),
    error => error instanceof RequestError && error.retryAfter === null,
  );
});

test("upload protocol headers override conflicting problem extensions", async () => {
  const { assertFreshUploadResponse, RequestError } = await import(
    "../../../assets/modules/http/client.js"
  );
  const bodyUploadId = "00000000-0000-4000-8000-000000000099";
  const response = new Response(JSON.stringify({
    status: 409,
    detail: "Upload was rejected",
    code: "upload_target_changed",
    recovery: "refresh_target",
    upload_id: bodyUploadId,
    upload_state: "running",
    upload_length: 999,
    upload_offset: 998,
  }), {
    status: 409,
    headers: {
      "content-type": "application/problem+json",
      "x-dufs-upload-id": uploadId,
      "x-dufs-operation-state": "rejected",
      "x-dufs-upload-length": "8",
    },
  });

  await assert.rejects(
    assertFreshUploadResponse(response, uploadId, 8),
    error => {
      assert.equal(error instanceof RequestError, true);
      assert.equal(error.uploadId, uploadId);
      assert.equal(error.uploadState, "rejected");
      assert.equal(error.uploadLength, 8);
      assert.equal(error.uploadOffset, null);
      assert.equal(error.upload.id, uploadId);
      assert.equal(error.upload.state, "rejected");
      assert.equal(error.upload.length, 8);
      assert.equal(error.upload.offset, null);
      return true;
    },
  );
});
