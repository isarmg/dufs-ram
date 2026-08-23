import assert from "node:assert/strict";
import test from "node:test";

import {
  classifyUploadResponse,
  parseTargetReplaceable,
  parseTargetRevision,
} from "../../../assets/modules/upload/protocol.js";
import {
  parseUploadPreflight,
} from "../../../assets/modules/upload/preflight.js";

const uploadId = "00000000-0000-4000-8000-000000000001";

test("upload response classifier binds success to status, ID and length", () => {
  assert.equal(classify({ status: 201, state: "committed" }).kind, "committed");
  assert.equal(classify({ status: 204, state: "committed" }).kind, "invalid");
  assert.equal(classify({
    status: 201,
    state: "committed",
    uploadId: "00000000-0000-4000-8000-000000000002",
  }).kind, "invalid");
  assert.equal(classify({
    status: 201,
    state: "committed",
    length: "9",
  }).kind, "invalid");
});

test("upload response classifier enforces the phase status-state matrix", () => {
  const matrix = {
    fresh: {
      running: [408, 409],
      "awaiting-confirmation": [409],
      committed: [200, 201],
      rejected: [408, 409, 413, 500, 507],
      "not-seen": [404],
      "not-started": [403, 404, 408, 409, 429, 503],
      unknown: [408, 500, 503, 504],
    },
    resume: {
      running: [408, 409, 413, 500, 507],
      "awaiting-confirmation": [409, 413],
      committed: [200, 204],
      rejected: [408, 409, 413, 500, 507],
      "not-seen": [404],
      "not-started": [403, 404, 408, 409, 429, 503],
      unknown: [408, 500, 503, 504],
    },
    checkpoint: {
      running: [200],
      "awaiting-confirmation": [409],
      committed: [200],
      rejected: [409],
      "not-seen": [404],
      "not-started": [],
      unknown: [500, 503],
    },
    discard: {
      running: [],
      "awaiting-confirmation": [],
      committed: [],
      rejected: [204],
      "not-seen": [],
      "not-started": [],
      unknown: [],
    },
  };
  const candidateStatuses = [
    200, 201, 202, 204, 206, 403, 404, 408, 409, 413, 418, 429, 500,
    503, 504, 507,
  ];

  for (const [phase, states] of Object.entries(matrix)) {
    for (const [state, acceptedStatuses] of Object.entries(states)) {
      for (const status of candidateStatuses) {
        const result = classify({ phase, status, state });
        const accepted = acceptedStatuses.includes(status);
        assert.equal(
          result.kind,
          accepted ? state : "invalid",
          `${phase}: HTTP ${status} with ${state}`,
        );
        if (accepted) {
          assert.equal(
            result.outcomeUnknown,
            state === "unknown" ||
              (state === "running" && phase !== "checkpoint"),
            `${phase}: certainty for HTTP ${status} with ${state}`,
          );
        }
      }
    }
  }

  assert.equal(classify({
    phase: "checkpoint",
    status: 200,
    state: "running",
    offset: "8",
  }).kind, "running");
  assert.equal(classify({
    phase: "checkpoint",
    status: 200,
    state: "committed",
    offset: "4",
  }).kind, "invalid");
  assert.equal(classify({
    phase: "discard",
    status: 204,
    state: "rejected",
    offset: null,
  }).kind, "invalid");
  assert.equal(classify({
    phase: "discard",
    status: 204,
    state: "rejected",
    offset: "4",
  }).kind, "invalid");
});

test("upload preflight binds ordered paths to typed target revisions", () => {
  const revision = "a".repeat(64);
  const targets = parseUploadPreflight({
    targets: [
      {
        path: "/folder/existing.txt",
        exists: true,
        revision,
        replaceable: true,
      },
      {
        path: "/folder/new.txt",
        exists: false,
        revision: null,
        replaceable: true,
      },
    ],
  }, ["/folder/existing.txt", "/folder/new.txt"]);

  assert.deepEqual(targets, [
    {
      path: "/folder/existing.txt",
      exists: true,
      revision,
      replaceable: true,
    },
    {
      path: "/folder/new.txt",
      exists: false,
      revision: null,
      replaceable: true,
    },
  ]);
  assert.equal(Object.isFrozen(targets), true);
  assert.equal(Object.isFrozen(targets[0]), true);
});

test("upload preflight rejects partial, reordered, or malformed authority", () => {
  const paths = ["/one.txt", "/two.txt"];
  const revision = "b".repeat(64);
  const valid = path => ({
    path,
    exists: true,
    revision,
    replaceable: true,
  });
  for (const payload of [
    { targets: [valid(paths[0])] },
    { targets: [valid(paths[1]), valid(paths[0])] },
    { targets: [valid(paths[0]), { ...valid(paths[1]), revision: "opaque" }] },
    { targets: [valid(paths[0]), { ...valid(paths[1]), exists: false }] },
  ]) {
    assert.throws(
      () => parseUploadPreflight(payload, paths),
      /Invalid upload preflight response/,
    );
  }
  assert.throws(
    () => parseUploadPreflight({
      targets: [valid(paths[0]), valid(paths[0])],
    }, [paths[0], paths[0]]),
    /Invalid upload preflight response/,
  );
});

test("overwrite authority headers require canonical revision and replaceable values", () => {
  const revision = "c".repeat(64);
  assert.equal(parseTargetRevision(new Headers({
    "X-Dufs-Target-Revision": revision,
  })), revision);
  for (const invalid of ["", "C".repeat(64), "c".repeat(63), "opaque"]) {
    assert.equal(parseTargetRevision(new Headers({
      "X-Dufs-Target-Revision": invalid,
    })), null);
  }
  assert.equal(parseTargetReplaceable(new Headers({
    "X-Dufs-Target-Replaceable": "true",
  })), true);
  assert.equal(parseTargetReplaceable(new Headers({
    "X-Dufs-Target-Replaceable": "false",
  })), false);
  assert.equal(parseTargetReplaceable(new Headers({
    "X-Dufs-Target-Replaceable": "1",
  })), null);
});
function classify(options) {
  const stateFields = {
    running: { length: "8", offset: "4" },
    "awaiting-confirmation": { length: "8", offset: "8" },
    committed: { length: "8", offset: "8" },
    rejected: { length: "8", offset: null },
    "not-seen": { length: null, offset: null },
    "not-started": { length: "8", offset: null },
    unknown: { length: null, offset: null },
  }[options.state];
  const length = options.length === undefined
    ? stateFields.length
    : options.length;
  const offset = options.offset === undefined
    ? options.phase === "discard" && options.state === "rejected"
      ? "8"
      : stateFields.offset
    : options.offset;
  const values = new Map([
    ["x-dufs-upload-id", options.uploadId || uploadId],
    ["x-dufs-operation-state", options.state],
  ]);
  if (length !== null) values.set("x-dufs-upload-length", length);
  if (offset !== null) values.set("x-dufs-upload-offset", offset);
  const result = classifyUploadResponse({
    phase: options.phase || "fresh",
    status: options.status,
    headers: name => values.get(name.toLowerCase()) || null,
    expectedUploadId: uploadId,
    expectedLength: 8,
  });
  return {
    kind: result.kind,
    outcomeUnknown: result.outcomeUnknown,
  };
}
