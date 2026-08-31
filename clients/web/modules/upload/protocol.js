import { parseUnsignedHeader } from "../http/headers.js";

export { parseUnsignedHeader } from "../http/headers.js";

export const OPERATION_STATE_HEADER = "X-Dufs-Operation-State";
export const UPLOAD_ID_HEADER = "X-Dufs-Upload-Id";
export const UPLOAD_LENGTH_HEADER = "X-Dufs-Upload-Length";
export const UPLOAD_OFFSET_HEADER = "X-Dufs-Upload-Offset";
export const UPLOAD_OVERWRITE_HEADER = "X-Dufs-Upload-Overwrite";
export const TARGET_REVISION_HEADER = "X-Dufs-Target-Revision";
export const TARGET_REPLACEABLE_HEADER = "X-Dufs-Target-Replaceable";

/** @typedef {"running" | "awaiting-confirmation" | "committed" | "rejected" | "not-seen" | "not-started" | "unknown"} UploadProtocolState */
/** @typedef {"fresh" | "resume" | "checkpoint" | "discard"} UploadRequestPhase */
/** @typedef {"absent" | "optional" | "required"} FieldPresence */
/** @typedef {{ length: FieldPresence, offset: FieldPresence }} UploadStateRule */
/** @typedef {{ uploadId: string, state: UploadProtocolState, length: number | null, offset: number | null }} BoundUploadProtocol */
/** @typedef {{ kind: "authentication" | "csrf" | "invalid" | UploadProtocolState, protocol: BoundUploadProtocol | null, outcomeUnknown: boolean }} UploadClassification */

export const FRESH_UPLOAD_SUCCESS_STATUSES = Object.freeze([200, 201]);
export const RESUME_UPLOAD_SUCCESS_STATUSES = Object.freeze([200, 204]);
export const FRESH_UPLOAD_ERROR_STATUSES = Object.freeze({
  running: Object.freeze([408, 409]),
  "awaiting-confirmation": Object.freeze([409]),
  rejected: Object.freeze([408, 409, 413, 500, 507]),
  "not-seen": Object.freeze([404]),
  "not-started": Object.freeze([403, 404, 408, 409, 429, 503]),
  unknown: Object.freeze([408, 500, 503, 504]),
});
export const RESUME_UPLOAD_ERROR_STATUSES = Object.freeze({
  running: Object.freeze([408, 409, 413, 500, 507]),
  "awaiting-confirmation": Object.freeze([408, 409, 413, 500, 507]),
  rejected: Object.freeze([408, 409, 413, 500, 507]),
  "not-seen": Object.freeze([404]),
  "not-started": Object.freeze([403, 404, 408, 409, 429, 503]),
  unknown: Object.freeze([408, 500, 503, 504]),
});
const CHECKPOINT_UPLOAD_STATUSES = Object.freeze({
  running: Object.freeze([200]),
  "awaiting-confirmation": Object.freeze([409]),
  committed: Object.freeze([200]),
  rejected: Object.freeze([409]),
  "not-seen": Object.freeze([404]),
  "not-started": Object.freeze([]),
  unknown: Object.freeze([429, 500, 503]),
});
const DISCARD_UPLOAD_STATUSES = Object.freeze({
  running: Object.freeze([]),
  "awaiting-confirmation": Object.freeze([]),
  committed: Object.freeze([]),
  rejected: Object.freeze([204]),
  "not-seen": Object.freeze([]),
  "not-started": Object.freeze([]),
  unknown: Object.freeze([]),
});

/** @type {Readonly<Record<UploadRequestPhase, Readonly<Record<UploadProtocolState, readonly number[]>>>>} */
export const UPLOAD_RESPONSE_STATUS_MATRIX = Object.freeze({
  fresh: Object.freeze({
    ...FRESH_UPLOAD_ERROR_STATUSES,
    committed: FRESH_UPLOAD_SUCCESS_STATUSES,
  }),
  resume: Object.freeze({
    ...RESUME_UPLOAD_ERROR_STATUSES,
    committed: RESUME_UPLOAD_SUCCESS_STATUSES,
  }),
  checkpoint: CHECKPOINT_UPLOAD_STATUSES,
  discard: DISCARD_UPLOAD_STATUSES,
});

const ABSENT = "absent";
const OPTIONAL = "optional";
const REQUIRED = "required";
/** @type {Readonly<Record<UploadProtocolState, Readonly<UploadStateRule>>>} */
const UPLOAD_STATE_RULES = Object.freeze({
  running: Object.freeze({
    length: REQUIRED,
    offset: REQUIRED,
  }),
  "awaiting-confirmation": Object.freeze({
    length: REQUIRED,
    offset: REQUIRED,
  }),
  committed: Object.freeze({
    length: REQUIRED,
    offset: REQUIRED,
  }),
  rejected: Object.freeze({
    length: REQUIRED,
    offset: OPTIONAL,
  }),
  "not-seen": Object.freeze({
    length: ABSENT,
    offset: ABSENT,
  }),
  "not-started": Object.freeze({
    length: REQUIRED,
    offset: OPTIONAL,
  }),
  unknown: Object.freeze({
    length: OPTIONAL,
    offset: OPTIONAL,
  }),
});

/** @type {readonly UploadProtocolState[]} */
export const UPLOAD_PROTOCOL_STATES = Object.freeze(
  /** @type {UploadProtocolState[]} */ (Object.keys(UPLOAD_STATE_RULES)),
);

const AUTH_ERROR_HEADER = "X-Dufs-Auth-Error";

/**
 * Classify one upload response using the complete HTTP-status/header matrix.
 * This function is deliberately side-effect free so XHR, fetch-based empty
 * uploads and checkpoint queries cannot drift into different interpretations.
 *
 * @param {{
 *   phase: UploadRequestPhase,
 *   status: number,
 *   headers: Headers | ((name: string) => string | null),
 *   expectedUploadId: string,
 *   expectedLength: number,
 * }} options
 */
export function classifyUploadResponse(options) {
  const {
    phase,
    status,
    headers,
    expectedUploadId,
    expectedLength,
  } = options;
  if (
    !Number.isSafeInteger(status) ||
    status < 100 ||
    status > 599 ||
    !Object.hasOwn(UPLOAD_RESPONSE_STATUS_MATRIX, phase)
  ) {
    return uploadClassification("invalid", null, true);
  }

  const authError = readHeader(headers, AUTH_ERROR_HEADER);
  if (status === 401) {
    return uploadClassification("authentication", null, false);
  }
  if (status === 403 && authError === "csrf") {
    return uploadClassification("csrf", null, false);
  }

  const protocol = parseBoundUploadProtocol(
    headers,
    expectedUploadId,
    expectedLength,
  );
  if (!protocol) return uploadClassification("invalid", null, true);

  const acceptedStatuses =
    UPLOAD_RESPONSE_STATUS_MATRIX[phase][protocol.state];
  if (
    !acceptedStatuses.includes(status) ||
    (
      phase === "checkpoint" &&
      status === 429 &&
      protocol.state === "unknown" &&
      (protocol.length !== null || protocol.offset !== null)
    ) ||
    (protocol.state === "committed" && protocol.offset !== expectedLength) ||
    (
      phase === "discard" &&
      protocol.state === "rejected" &&
      protocol.offset !== expectedLength
    )
  ) {
    return uploadClassification("invalid", protocol, true);
  }

  return uploadClassification(
    protocol.state,
    protocol,
    protocol.state === "unknown" ||
      (protocol.state === "running" && phase !== "checkpoint"),
  );
}

/**
 * @param {UploadClassification["kind"]} kind
 * @param {BoundUploadProtocol | null} protocol
 * @param {boolean} outcomeUnknown
 * @returns {Readonly<UploadClassification>}
 */
function uploadClassification(kind, protocol, outcomeUnknown) {
  return Object.freeze({ kind, protocol, outcomeUnknown });
}

/**
 * Parse and validate an upload response against the selected file length.
 *
 * @param {Headers | ((name: string) => string | null)} headers
 * @param {string} expectedUploadId
 * @param {number} expectedLength
 * @returns {BoundUploadProtocol | null}
 */
export function parseBoundUploadProtocol(
  headers,
  expectedUploadId,
  expectedLength,
) {
  if (!Number.isSafeInteger(expectedLength) || expectedLength < 0) return null;
  const uploadId = readHeader(headers, UPLOAD_ID_HEADER) || "";
  const state = readHeader(headers, OPERATION_STATE_HEADER) || "";
  if (
    uploadId !== expectedUploadId ||
    !isUploadProtocolState(state)
  ) {
    return null;
  }
  const rule = UPLOAD_STATE_RULES[state];
  const rawLength = readHeader(headers, UPLOAD_LENGTH_HEADER);
  const rawOffset = readHeader(headers, UPLOAD_OFFSET_HEADER);
  const length = parseUnsignedHeader(rawLength);
  const offset = parseUnsignedHeader(rawOffset);
  if (!matchesBoundField(
    rawLength,
    length,
    rule.length,
    expectedLength,
    false,
  )) {
    return null;
  }
  if (!matchesBoundField(
    rawOffset,
    offset,
    rule.offset,
    expectedLength,
    true,
  )) {
    return null;
  }
  return { uploadId, state, length, offset };
}

/**
 * Read a target revision only when it is a canonical 256-bit lowercase token.
 * The strict representation prevents a malformed conflict response from ever
 * becoming permission to overwrite a destination.
 *
 * @param {Headers | ((name: string) => string | null)} headers
 * @returns {string | null}
 */
export function parseTargetRevision(headers) {
  const revision = readHeader(headers, TARGET_REVISION_HEADER);
  return typeof revision === "string" && /^[0-9a-f]{64}$/.test(revision)
    ? revision
    : null;
}

/**
 * @param {Headers | ((name: string) => string | null)} headers
 * @returns {boolean | null}
 */
export function parseTargetReplaceable(headers) {
  const replaceable = readHeader(headers, TARGET_REPLACEABLE_HEADER);
  if (replaceable === "true") return true;
  if (replaceable === "false") return false;
  return null;
}

/**
 * @param {string | null} rawValue
 * @param {number | null} parsedValue
 * @param {FieldPresence} presence
 * @param {number} expectedLength
 * @param {boolean} isOffset
 */
function matchesBoundField(
  rawValue,
  parsedValue,
  presence,
  expectedLength,
  isOffset,
) {
  if (presence === ABSENT) return rawValue === null;
  if (rawValue === null) return presence === OPTIONAL;
  if (parsedValue === null) return false;
  return isOffset
    ? parsedValue <= expectedLength
    : parsedValue === expectedLength;
}

/**
 * @param {Headers | ((name: string) => string | null)} headers
 * @param {string} name
 */
function readHeader(headers, name) {
  return typeof headers === "function" ? headers(name) : headers.get(name);
}

/** @param {string} value @returns {value is UploadProtocolState} */
function isUploadProtocolState(value) {
  return Object.hasOwn(UPLOAD_STATE_RULES, value);
}
