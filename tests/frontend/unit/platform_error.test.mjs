import assert from "node:assert/strict";
import test from "node:test";
import { platformErrorCode } from "../../../clients/web/modules/http/platform-error.js";
import { classifyUploadResponse } from "../../../clients/web/modules/upload/protocol.js";
import { responsePlatformErrorCode, ERROR_RESPONSE_BODY_LIMIT } from "../../../clients/web/modules/http/client.js";

const envelope = JSON.stringify({ code: "auth.csrf_rejected", message: "Request rejected", retryable: false });
test("all upload phases classify current CSRF rejection before business state", () => {
  const errorCode = platformErrorCode(envelope, "application/json; charset=utf-8");
  assert.equal(errorCode, "auth.csrf_rejected");
  for (const phase of ["fresh", "resume", "discard", "checkpoint"]) {
    const classified = classifyUploadResponse({ phase, status: 403, headers: new Headers(), errorCode, expectedUploadId: "00000000-0000-4000-8000-000000000040", expectedLength: 10 });
    assert.deepEqual(classified, { kind: "csrf", protocol: null, outcomeUnknown: false });
  }
  for (const text of ["", "{}", JSON.stringify({ code: "auth.csrf_rejected" }), "<html>failure</html>"]) assert.equal(platformErrorCode(text, "application/json"), null);
  assert.equal(platformErrorCode(envelope, "text/html"), null);
});

test("403 classification consumes only a bounded body and preserves the business response", async () => {
  const response = new Response(envelope, { status: 403, headers: { "content-type": "application/json" } });
  assert.equal(await responsePlatformErrorCode(response), "auth.csrf_rejected");
  assert.equal(await responsePlatformErrorCode(response), "auth.csrf_rejected");
  assert.equal(await response.text(), envelope);
  const tooLarge = new Response("x".repeat(ERROR_RESPONSE_BODY_LIMIT + 1), { status: 403, headers: { "content-type": "application/json" } });
  await assert.rejects(responsePlatformErrorCode(tooLarge), error => error.outcomeUnknown === true);
});
