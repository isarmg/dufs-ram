import assert from "node:assert/strict";
import test from "node:test";

import { RequestError } from "../../../assets/modules/http/client.js";
import {
  trackedMutationEffect,
  uploadMutationEffect,
} from "../../../assets/modules/operations/file_operations.js";
import {
  MUTATION_EFFECT,
} from "../../../assets/modules/shared/mutation_effect.js";

const uploadId = "00000000-0000-4000-8000-000000000001";

test("mutation invalidation distinguishes committed, unknown, and rejected writes", () => {
  assert.equal(
    trackedMutationEffect({
      kind: "succeeded",
      response: null,
      status: null,
      reconciled: false,
    }),
    MUTATION_EFFECT.COMMITTED,
  );
  const unknown = new RequestError("dispatch outcome unknown", {
    outcomeUnknown: true,
  });
  assert.equal(
    trackedMutationEffect({ kind: "failed", error: unknown, status: null }),
    MUTATION_EFFECT.OUTCOME_UNKNOWN,
  );
  assert.equal(
    trackedMutationEffect({
      kind: "failed",
      error: unknown,
      status: {
        kind: "unknown",
        jobId: uploadId,
        state: "unknown",
        status: 500,
        code: "outcome_uncertain",
        message: "unknown",
        authenticationFailed: false,
      },
    }),
    MUTATION_EFFECT.OUTCOME_UNKNOWN,
  );
  assert.equal(
    trackedMutationEffect({
      kind: "failed",
      error: unknown,
      status: {
        kind: "failed",
        jobId: uploadId,
        state: "failed",
        status: 409,
        code: "path_exists",
        message: "failed",
        authenticationFailed: false,
      },
    }),
    MUTATION_EFFECT.NOT_COMMITTED,
  );
  const preDispatch = new RequestError("cancelled before dispatch", {
    outcomeUnknown: false,
  });
  assert.equal(
    trackedMutationEffect({
      kind: "failed",
      error: preDispatch,
      status: null,
    }),
    MUTATION_EFFECT.NOT_COMMITTED,
  );

  assert.equal(
    uploadMutationEffect(unknown, null),
    MUTATION_EFFECT.OUTCOME_UNKNOWN,
  );
  assert.equal(
    uploadMutationEffect(unknown, {
      operationId: uploadId,
      state: "committed",
      status: 200,
      code: "",
      message: "committed",
      authenticationFailed: false,
    }),
    MUTATION_EFFECT.COMMITTED,
  );
  assert.equal(
    uploadMutationEffect(unknown, {
      operationId: uploadId,
      state: "rejected",
      status: 409,
      code: "",
      message: "rejected",
      authenticationFailed: false,
    }),
    MUTATION_EFFECT.NOT_COMMITTED,
  );
  assert.equal(
    uploadMutationEffect(unknown, {
      operationId: uploadId,
      state: "not-seen",
      status: 404,
      code: "",
      message: "not seen yet",
      authenticationFailed: false,
    }),
    MUTATION_EFFECT.OUTCOME_UNKNOWN,
  );
});
