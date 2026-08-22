import {
  OPERATION_STATE_HEADER,
  UPLOAD_ID_HEADER,
  classifyUploadResponse,
} from "../upload/protocol.js";
import {
  ERROR_RESPONSE_BODY_LIMIT,
  SUCCESS_RESPONSE_BODY_LIMIT,
  bufferResponse as bufferBoundedResponse,
} from "./response_buffer.js";

export {
  ERROR_RESPONSE_BODY_LIMIT,
  SUCCESS_RESPONSE_BODY_LIMIT,
} from "./response_buffer.js";

export const CSRF_HEADER = "X-Dufs-CSRF-Token";
export const AUTH_ERROR_HEADER = "X-Dufs-Auth-Error";
export const AUTH_REQUIRED_MESSAGE =
  "Your session is no longer valid. Returning to the sign-in page.";
export const PAGE_EXPIRED_MESSAGE =
  "Your session or this page is no longer valid. Refresh the page and select the files again.";
export const RESULT_UNKNOWN_MESSAGE =
  "The result is unknown; refresh the folder to verify what happened before trying again.";
export const REQUEST_TIMEOUT_MS = 30 * 1000;
export const OPERATION_ID_HEADER = "X-Dufs-Operation-Id";
export { OPERATION_STATE_HEADER };

const ERROR_MESSAGE_LIMIT = 1024;
const ERROR_TYPE_LIMIT = 2048;
const ERROR_CODE_PATTERN = /^[a-z][a-z0-9_]{0,63}$/;
const ERROR_STATE_PATTERN = /^[a-z][a-z0-9-]{0,63}$/;
const RECOVERY_VALUES = new Set([
  "retry",
  "retry_with_new_id",
  "resume_upload",
  "query_job",
  "query_upload",
  "refresh_target",
]);
const OPERATION_ID_PATTERN =
  /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

/**
 * @typedef {Object} OperationMetadata
 * @property {string} id
 * @property {string} state
 * @property {number} httpStatus
 */

/**
 * @typedef {Object} UploadMetadata
 * @property {string} id
 * @property {string} state
 * @property {number | null} length
 * @property {number | null} offset
 */

/**
 * The validated subset of the upload response protocol used by this module.
 *
 * @typedef {Object} BoundUploadProtocol
 * @property {string} uploadId
 * @property {string} state
 * @property {number | null} length
 * @property {number | null} offset
 */

/**
 * @typedef {Object} ParsedErrorPayload
 * @property {string} type
 * @property {string} title
 * @property {number} status
 * @property {string} detail
 * @property {string} code
 * @property {string} message
 * @property {string} recovery
 * @property {number | null} retryAfter
 * @property {Readonly<OperationMetadata> | null} operation
 * @property {Readonly<UploadMetadata> | null} upload
 */

/**
 * @typedef {Object} RequestErrorOptions
 * @property {number} [status]
 * @property {number} [problemStatus]
 * @property {string} [code]
 * @property {string} [kind]
 * @property {string} [type]
 * @property {string} [title]
 * @property {string} [detail]
 * @property {string} [recovery]
 * @property {number | null} [retryAfter]
 * @property {boolean} [outcomeUnknown]
 * @property {string} [operationId]
 * @property {string} [operationState]
 * @property {number} [operationHttpStatus]
 * @property {Readonly<OperationMetadata> | null} [operation]
 * @property {string} [uploadId]
 * @property {string} [uploadState]
 * @property {number | null} [uploadLength]
 * @property {number | null} [uploadOffset]
 * @property {Readonly<UploadMetadata> | null} [upload]
 */

/**
 * @typedef {Object} RequestOptions
 * @property {number} [timeoutMs]
 * @property {string} [timeoutMessage]
 * @property {boolean} [outcomeUnknown]
 * @property {string} [operationId]
 * @property {string} [resultId]
 * @property {number} [limit]
 */

/**
 * @typedef {Object} RequestJsonResult
 * @property {Response} response
 * @property {unknown} payload
 */

export class RequestError extends Error {
  /**
   * @param {string} message
   * @param {RequestErrorOptions} [options]
   */
  constructor(message, options = {}) {
    super(message);
    this.name = "RequestError";
    this.status = options.status || 0;
    this.problemStatus = options.problemStatus || 0;
    this.code = options.code || "";
    this.kind = options.kind || "request";
    this.type = options.type || "";
    this.title = options.title || "";
    this.detail = options.detail || "";
    this.recovery = options.recovery || "";
    this.retryAfter = options.retryAfter ?? null;
    this.outcomeUnknown = Boolean(options.outcomeUnknown);
    this.operationId = options.operationId || "";
    this.operationState = options.operationState || "";
    this.operationHttpStatus = options.operationHttpStatus || 0;
    this.operation = options.operation || null;
    this.uploadId = options.uploadId || "";
    this.uploadState = options.uploadState || "";
    this.uploadLength = options.uploadLength ?? null;
    this.uploadOffset = options.uploadOffset ?? null;
    this.upload = options.upload || null;
  }
}

/**
 * @param {Response} response
 * @param {string} [method]
 * @param {{limit?: number, outcomeUnknown?: boolean, operationId?: string}} [options]
 * @returns {Promise<Response>}
 */
export async function bufferResponse(response, method = "GET", options = {}) {
  return await bufferBoundedResponse(
    response,
    method,
    options,
    (message, errorOptions) => new RequestError(message, errorOptions),
  );
}

class AuthenticationError extends RequestError {
  /** @param {string} message @param {RequestErrorOptions} [options] */
  constructor(message, options) {
    super(message, options);
    this.name = "AuthenticationError";
  }
}

/** @param {number} status @param {string | null} authError */
export function isCsrfAuthFailure(status, authError) {
  return status === 403 && authError === "csrf";
}

/** @param {number} status @param {string | null} authError */
export function authFailureMessage(status, authError) {
  if (status === 401) return AUTH_REQUIRED_MESSAGE;
  if (isCsrfAuthFailure(status, authError)) return PAGE_EXPIRED_MESSAGE;
  return "";
}

/**
 * @param {Response} response
 * @param {(() => void) | undefined} [onUnauthorized]
 * @returns {Promise<Response>}
 */
export async function assertResponse(response, onUnauthorized) {
  const operationId = response.headers.get(OPERATION_ID_HEADER) || "";
  const operationState =
    response.headers.get(OPERATION_STATE_HEADER) || "";
  const authMessage = authFailureMessage(
    response.status,
    response.headers.get(AUTH_ERROR_HEADER),
  );
  if (authMessage) {
    onUnauthorized?.();
    throw new AuthenticationError(authMessage, {
      status: response.status,
      code: response.status === 401
        ? "authentication_required"
        : "csrf_failed",
      kind: response.status === 401 ? "authentication" : "csrf",
      operationId,
      operationState,
    });
  }
  if (response.ok && !operationState) return response;
  if (response.ok && operationState === "succeeded") return response;
  const validOperationStates = [
    "running",
    "succeeded",
    "failed",
    "rejected",
    "unknown",
    "committed",
    "not-seen",
  ];
  const contradictoryOperationState =
    Boolean(operationState) &&
    (
      !validOperationStates.includes(operationState) ||
      (response.ok && operationState !== "succeeded") ||
      (
        !response.ok &&
        ["succeeded", "committed"].includes(operationState)
      )
    );
  if (contradictoryOperationState) {
    throw new RequestError(
      `Invalid operation result. ${RESULT_UNKNOWN_MESSAGE}`,
      {
        status: response.status,
        code: "invalid_operation_result",
        kind: "protocol",
        outcomeUnknown: true,
        operationId,
        operationState: "unknown",
      },
    );
  }
  const detail = parseErrorPayload(
    await requestTextError(response),
    response.headers.get("content-type") || "",
  );
  const outcomeUnknown = ["running", "unknown"].includes(operationState);
  const errorMetadata = {
    ...requestErrorMetadata(detail, response.headers),
    ...authoritativeOperationMetadata(
      operationId,
      operationState,
      detail.operation,
    ),
  };
  if (detail.status && detail.status !== response.status) {
    throw new RequestError(
      "Invalid error response: problem status does not match HTTP status",
      {
        ...errorMetadata,
        status: response.status,
        code: "invalid_problem_status",
        kind: "protocol",
        outcomeUnknown,
        operationId: operationId || detail.operation?.id || "",
        operationState: operationState || detail.operation?.state || "",
      },
    );
  }
  throw new RequestError(
    detail.message || `Request failed (HTTP ${response.status})`,
    {
      ...errorMetadata,
      status: response.status,
      code: detail.code,
      kind: response.status === 409 ? "conflict" : "http",
      outcomeUnknown,
      operationId: operationId || detail.operation?.id || "",
      operationState: operationState || detail.operation?.state || "",
    },
  );
}

/**
 * @param {Response} response
 * @param {string} expectedUploadId
 * @param {number} expectedLength
 * @param {(() => void) | undefined} [onUnauthorized]
 * @returns {Promise<Response>}
 */
export async function assertFreshUploadResponse(
  response,
  expectedUploadId,
  expectedLength,
  onUnauthorized,
) {
  const classification = classifyUploadResponse({
    phase: "fresh",
    status: response.status,
    headers: response.headers,
    expectedUploadId,
    expectedLength,
  });
  if (["authentication", "csrf"].includes(classification.kind)) {
    return assertResponse(response, onUnauthorized);
  }
  if (classification.kind === "committed") return response;
  if (classification.kind !== "invalid") {
    const detail = parseErrorPayload(
      await requestTextError(response),
      response.headers.get("content-type") || "",
    );
    const errorMetadata = {
      ...requestErrorMetadata(detail, response.headers),
      ...authoritativeUploadMetadata(
        classification.protocol,
        detail.upload,
      ),
    };
    if (detail.status && detail.status !== response.status) {
      throw new RequestError(
        "Invalid error response: problem status does not match HTTP status",
        {
          ...errorMetadata,
          status: response.status,
          code: "invalid_problem_status",
          kind: "protocol",
          outcomeUnknown: classification.outcomeUnknown,
          operationId: expectedUploadId,
          operationState: classification.kind,
          uploadId: expectedUploadId,
          uploadState: classification.kind,
        },
      );
    }
    throw new RequestError(
      detail.message || `Upload failed (HTTP ${response.status})`,
      {
        ...errorMetadata,
        status: response.status,
        code: detail.code,
        kind: response.status === 409 ? "conflict" : "http",
        outcomeUnknown: classification.outcomeUnknown,
        operationId: expectedUploadId,
        operationState: classification.kind,
        uploadId: expectedUploadId,
        uploadState: classification.kind,
      },
    );
  }
  throw new RequestError(
    `Invalid upload result. ${RESULT_UNKNOWN_MESSAGE}`,
    {
      status: response.status,
      code: "invalid_upload_result",
      kind: "protocol",
      outcomeUnknown: true,
      operationId: expectedUploadId,
      operationState: "unknown",
    },
  );
}

/** @param {unknown} error */
export function isAuthenticationError(error) {
  return error instanceof AuthenticationError;
}

/** @param {unknown} error @param {string} code */
export function isRequestErrorCode(error, code) {
  return error instanceof RequestError && error.code === code;
}

/**
 * @param {unknown} rawBody
 * @param {string} [contentType]
 * @returns {Readonly<ParsedErrorPayload>}
 */
export function parseErrorPayload(rawBody, contentType = "") {
  const body = typeof rawBody === "string" ? rawBody.trim() : "";
  let code = "";
  let message = "";
  let type = "";
  let title = "";
  let status = 0;
  let detail = "";
  let recovery = "";
  let retryAfter = null;
  let operation = null;
  let upload = null;
  if (body && isProblemMediaType(contentType)) {
    try {
      const payload = JSON.parse(body);
      if (isRecord(payload)) {
        type = normalizeBoundedString(payload.type, ERROR_TYPE_LIMIT);
        title = normalizeErrorMessageValue(payload.title);
        status = normalizeHttpStatus(payload.status);
        detail = normalizeErrorMessageValue(payload.detail);
        code = normalizeErrorCode(payload.code);
        recovery = normalizeRecoveryAdvice(payload.recovery);
        retryAfter = normalizeUnsignedInteger(payload.retry_after);
        operation = parseOperationExtension(payload);
        upload = parseUploadExtension(payload);
        message = detail || title;
      }
    } catch {}
  }
  return Object.freeze({
    type,
    title,
    status,
    detail,
    code,
    message,
    recovery,
    retryAfter,
    operation,
    upload,
  });
}

/**
 * @param {Readonly<ParsedErrorPayload>} detail
 * @param {Headers | undefined} headers
 * @returns {RequestErrorOptions}
 */
function requestErrorMetadata(detail, headers) {
  const retryAfterHeader = headers?.get("retry-after");
  return {
    problemStatus: detail.status,
    type: detail.type,
    title: detail.title,
    detail: detail.detail,
    recovery: detail.recovery,
    // Retry-After is transport metadata and therefore authoritative when the
    // server also repeats the delta in the Problem Details extension.
    retryAfter: retryAfterHeader === null || retryAfterHeader === undefined
      ? detail.retryAfter
      : normalizeRetryAfterHeader(retryAfterHeader),
    operationHttpStatus: detail.operation?.httpStatus || 0,
    operation: detail.operation,
    uploadId: detail.upload?.id || "",
    uploadState: detail.upload?.state || "",
    uploadLength: detail.upload?.length ?? null,
    uploadOffset: detail.upload?.offset ?? null,
    upload: detail.upload,
  };
}

/**
 * @param {BoundUploadProtocol | null} protocol
 * @param {Readonly<UploadMetadata> | null} bodyUpload
 * @returns {RequestErrorOptions}
 */
function authoritativeUploadMetadata(protocol, bodyUpload) {
  if (!protocol) return {};
  const upload = Object.freeze({
    ...(bodyUpload || {}),
    id: protocol.uploadId,
    state: protocol.state,
    length: protocol.length,
    offset: protocol.offset,
  });
  return {
    uploadId: protocol.uploadId,
    uploadState: protocol.state,
    uploadLength: protocol.length,
    uploadOffset: protocol.offset,
    upload,
  };
}

/**
 * @param {string} operationId
 * @param {string} operationState
 * @param {Readonly<OperationMetadata> | null} bodyOperation
 * @returns {RequestErrorOptions}
 */
function authoritativeOperationMetadata(operationId, operationState, bodyOperation) {
  if (!operationId && !operationState) return {};
  const operation = Object.freeze({
    id: operationId || bodyOperation?.id || "",
    state: operationState || bodyOperation?.state || "",
    httpStatus: bodyOperation?.httpStatus || 0,
  });
  return {
    operationId: operation.id,
    operationState: operation.state,
    operationHttpStatus: operation.httpStatus || 0,
    operation,
  };
}

/**
 * @param {Record<string, unknown>} payload
 * @returns {Readonly<OperationMetadata> | null}
 */
function parseOperationExtension(payload) {
  const id = normalizeOperationId(payload.operation_id);
  const state = normalizeErrorState(payload.state);
  if (!id || !state) return null;
  return Object.freeze({
    id,
    state,
    httpStatus: normalizeHttpStatus(payload.http_status),
  });
}

/**
 * @param {Record<string, unknown>} payload
 * @returns {Readonly<UploadMetadata> | null}
 */
function parseUploadExtension(payload) {
  const id = normalizeOperationId(payload.upload_id);
  const state = normalizeErrorState(payload.upload_state);
  if (!id || !state) return null;
  return Object.freeze({
    id,
    state,
    length: normalizeUnsignedInteger(payload.upload_length),
    offset: normalizeUnsignedInteger(payload.upload_offset),
  });
}

/** @param {unknown} value @returns {value is Record<string, unknown>} */
function isRecord(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

/** @param {unknown} value @returns {value is JobState} */
function isJobState(value) {
  return typeof value === "string" &&
    ["running", "succeeded", "failed", "unknown"].includes(value);
}

/** @param {unknown} value @returns {string} */
function normalizeErrorCode(value) {
  return typeof value === "string" && ERROR_CODE_PATTERN.test(value)
    ? value
    : "";
}

/** @param {unknown} value @returns {string} */
function normalizeErrorState(value) {
  return typeof value === "string" && ERROR_STATE_PATTERN.test(value)
    ? value
    : "";
}

/** @param {unknown} value @returns {string} */
function normalizeRecoveryAdvice(value) {
  return typeof value === "string" && RECOVERY_VALUES.has(value) ? value : "";
}

/** @param {unknown} value @returns {string} */
function normalizeOperationId(value) {
  return typeof value === "string" && OPERATION_ID_PATTERN.test(value)
    ? value
    : "";
}

/** @param {unknown} value @returns {number} */
function normalizeHttpStatus(value) {
  return typeof value === "number" &&
      Number.isInteger(value) &&
      value >= 100 &&
      value <= 599
    ? value
    : 0;
}

/** @param {unknown} value @returns {number | null} */
function normalizeUnsignedInteger(value) {
  return typeof value === "number" &&
      Number.isSafeInteger(value) &&
      value >= 0
    ? value
    : null;
}

/** @param {unknown} value @returns {number | null} */
function normalizeRetryAfterHeader(value) {
  if (typeof value !== "string" || !/^(0|[1-9][0-9]*)$/.test(value)) {
    return null;
  }
  const seconds = Number(value);
  return Number.isSafeInteger(seconds) ? seconds : null;
}

/** @param {unknown} value @returns {string} */
function normalizeErrorMessageValue(value) {
  return typeof value === "string" ? normalizeErrorMessage(value) : "";
}

/** @param {unknown} value @param {number} limit @returns {string} */
function normalizeBoundedString(value, limit) {
  if (typeof value !== "string") return "";
  const normalized = value.trim();
  return normalized && normalized.length <= limit ? normalized : "";
}

/**
 * Fetch and parse one bounded JSON response while the request deadline remains
 * active. The response is returned alongside the payload so callers can still
 * validate status and protocol headers.
 *
 * @param {RequestInfo | URL} url
 * @param {RequestInit} [init]
 * @param {RequestOptions} [options]
 * @returns {Promise<Readonly<RequestJsonResult>>}
 */
export async function requestJson(url, init = {}, options = {}) {
  return await performRequest(url, init, options, async response => {
    const buffered = await bufferResponse(response, init.method, {
      outcomeUnknown: Boolean(options.outcomeUnknown),
      operationId: options.resultId || options.operationId || "",
      limit: options.limit,
    });
    if (!buffered.ok) {
      return Object.freeze({ response: buffered, payload: null });
    }
    if (!isJsonMediaType(buffered.headers.get("content-type"))) {
      await buffered.body?.cancel();
      throw new RequestError("The server returned a non-JSON response", {
        status: response.status,
        code: "invalid_json_content_type",
        kind: "protocol",
        outcomeUnknown: Boolean(options.outcomeUnknown),
        operationId: options.resultId || options.operationId || "",
        operationState: options.outcomeUnknown ? "unknown" : "",
      });
    }
    let payload;
    try {
      // Consume the bounded replay stream directly. Cloning it would tee the
      // stream and retain a second copy of every chunk in an unread branch.
      payload = JSON.parse(await buffered.text());
    } catch {
      throw new RequestError("The server returned invalid JSON", {
        status: response.status,
        code: "invalid_json_response",
        kind: "protocol",
        outcomeUnknown: Boolean(options.outcomeUnknown),
        operationId: options.resultId || options.operationId || "",
        operationState: options.outcomeUnknown ? "unknown" : "",
      });
    }
    return Object.freeze({ response: buffered, payload });
  });
}

/**
 * Fetch a response that is expected to have no success body.
 *
 * @param {RequestInfo | URL} url
 * @param {RequestInit} [init]
 * @param {RequestOptions} [options]
 * @returns {Promise<Response>}
 */
export async function requestNoContent(url, init = {}, options = {}) {
  return await performRequest(url, init, options, async response => {
    const buffered = await bufferResponse(response, init.method, {
      outcomeUnknown: Boolean(options.outcomeUnknown),
      operationId: options.resultId || options.operationId || "",
      limit: ERROR_RESPONSE_BODY_LIMIT,
    });
    if (
      buffered.ok &&
      ![204, 205].includes(buffered.status) &&
      (await buffered.arrayBuffer()).byteLength !== 0
    ) {
      throw new RequestError("The server returned an unexpected response body", {
        status: buffered.status,
        code: "unexpected_response_body",
        kind: "protocol",
        outcomeUnknown: Boolean(options.outcomeUnknown),
        operationId: options.resultId || options.operationId || "",
        operationState: options.outcomeUnknown ? "unknown" : "",
      });
    }
    return buffered;
  });
}

/** @param {unknown} rawContentType @returns {boolean} */
function isJsonMediaType(rawContentType) {
  const mediaType = String(rawContentType || "")
    .split(";", 1)[0]
    .trim()
    .toLowerCase();
  return mediaType === "application/json" ||
    (mediaType.startsWith("application/") && mediaType.endsWith("+json"));
}

/** @param {unknown} rawContentType @returns {boolean} */
function isProblemMediaType(rawContentType) {
  return String(rawContentType || "")
    .split(";", 1)[0]
    .trim()
    .toLowerCase() === "application/problem+json";
}

/**
 * Fetch headers only; callers cannot accidentally extend the deadline gap.
 *
 * @param {RequestInfo | URL} url
 * @param {RequestInit} [init]
 * @param {RequestOptions} [options]
 * @returns {Promise<Response>}
 */
export async function requestHead(url, init = {}, options = {}) {
  return await performRequest(
    url,
    { ...init, method: "HEAD" },
    options,
    async response => response,
  );
}

/**
 * @template T
 * @param {RequestInfo | URL} url
 * @param {RequestInit} init
 * @param {RequestOptions} options
 * @param {(response: Response) => Promise<T>} consume
 * @returns {Promise<T>}
 */
async function performRequest(url, init, options, consume) {
  const timeoutMs = options.timeoutMs ?? REQUEST_TIMEOUT_MS;
  if (!Number.isSafeInteger(timeoutMs) || timeoutMs <= 0) {
    throw new TypeError("Request timeout must be a positive integer");
  }

  const callerSignal = init.signal;
  const outcomeUnknown = Boolean(options.outcomeUnknown);
  const operationId = options.operationId || "";
  const resultId = options.resultId || operationId;
  // No request can have reached the server when the caller supplies a signal
  // that was already aborted. Keep this distinct from cancellation after the
  // fetch dispatch, whose operation outcome must remain conservative.
  if (callerSignal?.aborted) {
    throw new RequestError("Request cancelled.", {
      code: "client_cancelled",
      kind: "cancelled",
      outcomeUnknown: false,
      operationId: resultId,
      operationState: "",
    });
  }

  const controller = new AbortController();
  let timedOut = false;
  const forwardAbort = () => controller.abort();
  callerSignal?.addEventListener("abort", forwardAbort, { once: true });
  const timer = window.setTimeout(() => {
    timedOut = true;
    controller.abort();
  }, timeoutMs);

  try {
    const response = await fetch(url, {
      ...init,
      signal: controller.signal,
    });
    const authMessage = authFailureMessage(
      response.status,
      response.headers.get(AUTH_ERROR_HEADER),
    );
    // Authentication happens before the operation registry on the server, so
    // these responses intentionally do not carry an operation envelope. Let
    // assertResponse classify them and invoke the caller's auth callback.
    if (operationId && !authMessage) {
      const returnedOperationId =
        response.headers.get(OPERATION_ID_HEADER) || "";
      const returnedOperationState =
        response.headers.get(OPERATION_STATE_HEADER) || "";
      const validOperationStates =
        ["running", "succeeded", "failed", "rejected", "unknown"];
      if (
        returnedOperationId !== operationId ||
        !validOperationStates.includes(returnedOperationState) ||
        (response.ok && returnedOperationState !== "succeeded") ||
        (!response.ok && returnedOperationState === "succeeded")
      ) {
        throw new RequestError(
          `Invalid operation result. ${RESULT_UNKNOWN_MESSAGE}`,
          {
            status: response.status,
            code: "invalid_operation_result",
            kind: "protocol",
            outcomeUnknown: true,
            operationId,
            operationState: "unknown",
          },
        );
      }
    }
    return await consume(response);
  } catch (error) {
    if (error instanceof RequestError) throw error;
    if (timedOut) {
      throw new RequestError(
        options.timeoutMessage ||
          (outcomeUnknown
            ? `Request timed out. ${RESULT_UNKNOWN_MESSAGE}`
            : "Request timed out. Try again."),
        {
          code: "client_timeout",
          kind: "timeout",
          outcomeUnknown,
          operationId: resultId,
          operationState: outcomeUnknown ? "unknown" : "",
        },
      );
    }
    if (controller.signal.aborted) {
      throw new RequestError(
        outcomeUnknown
          ? `Request cancelled. ${RESULT_UNKNOWN_MESSAGE}`
          : "Request cancelled.",
        {
          code: "client_cancelled",
          kind: "cancelled",
          outcomeUnknown,
          operationId: resultId,
          operationState: outcomeUnknown ? "unknown" : "",
        },
      );
    }
    throw new RequestError(
      outcomeUnknown
        ? `Network connection lost. ${RESULT_UNKNOWN_MESSAGE}`
        : "Network connection lost. Try again.",
      {
        code: "network_error",
        kind: "network",
        outcomeUnknown,
        operationId: resultId,
        operationState: outcomeUnknown ? "unknown" : "",
      },
    );
  } finally {
    window.clearTimeout(timer);
    callerSignal?.removeEventListener("abort", forwardAbort);
  }
}

/**
 * @param {RequestInfo | URL} url
 * @param {string} csrfToken
 * @param {unknown} body
 * @param {RequestOptions} [options]
 * @returns {Promise<Response>}
 */
export function postJson(url, csrfToken, body, options = {}) {
  const operationId = options.operationId || crypto.randomUUID();
  return requestNoContent(url, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      [CSRF_HEADER]: csrfToken,
      [OPERATION_ID_HEADER]: operationId,
    },
    body: JSON.stringify(body),
  }, {
    ...options,
    outcomeUnknown: options.outcomeUnknown ?? true,
    operationId,
  });
}

/**
 * Fields shared by every successful job-status classification.
 *
 * @typedef {Object} JobStatusFields
 * @property {string} jobId
 * @property {number} status
 * @property {string} code
 * @property {string} message
 */

/** @typedef {"running" | "succeeded" | "failed" | "unknown"} JobState */

/**
 * @typedef {
 *   | (JobStatusFields & {
 *       kind: "running",
 *       state: "running",
 *       authenticationFailed: false,
 *     })
 *   | (JobStatusFields & {
 *       kind: "succeeded",
 *       state: "succeeded",
 *       authenticationFailed: false,
 *     })
 *   | (JobStatusFields & {
 *       kind: "failed",
 *       state: "failed",
 *       authenticationFailed: false,
 *     })
 *   | (JobStatusFields & {
 *       kind: "unknown",
 *       state: "unknown",
 *       authenticationFailed: false,
 *     })
 *   | (JobStatusFields & {
 *       kind: "authentication",
 *       state: "unknown",
 *       authenticationFailed: true,
 *     })
 *   | (JobStatusFields & {
 *       kind: "unavailable",
 *       state: "unknown",
 *       authenticationFailed: false,
 *     })
 * } JobQueryResult
 */

/**
 * @typedef {
 *   | {
 *       kind: "succeeded",
 *       response: Response | null,
 *       status: JobQueryResult | null,
 *       reconciled: boolean,
 *     }
 *   | { kind: "authentication" }
 *   | {
 *       kind: "failed",
 *       error: unknown,
 *       status: JobQueryResult | null,
 *     }
 * } MutationResult
 */

/**
 * Execute one mutation and reconcile an indeterminate response exactly once.
 *
 * @param {() => Promise<Response>} execute
 * @param {(() => void) | undefined} onUnauthorized
 * @returns {Promise<MutationResult>}
 */
export async function runMutationWithReconciliation(execute, onUnauthorized) {
  try {
    const response = await execute();
    await assertResponse(response, onUnauthorized);
    return Object.freeze({
      kind: "succeeded",
      response,
      status: null,
      reconciled: false,
    });
  } catch (error) {
    if (isAuthenticationError(error)) {
      return Object.freeze({ kind: "authentication" });
    }
    const jobId = jobIdForUnknownMutation(error);
    const status = jobId ? await queryJob(jobId, onUnauthorized) : null;
    if (status?.kind === "authentication") {
      return Object.freeze({ kind: "authentication" });
    }
    if (status?.kind === "succeeded") {
      return Object.freeze({
        kind: "succeeded",
        response: null,
        status,
        reconciled: true,
      });
    }
    return Object.freeze({ kind: "failed", error, status });
  }
}

/**
 * Query a job once and classify every possible result with `kind`.
 *
 * @param {string} jobId
 * @param {(() => void) | undefined} [onUnauthorized]
 * @returns {Promise<JobQueryResult>}
 */
export async function queryJob(jobId, onUnauthorized) {
  if (!OPERATION_ID_PATTERN.test(jobId)) {
    throw new TypeError("Job ID must be a canonical UUID");
  }

  try {
    const { response, payload } = await requestJson(
      `/__dufs__/api/jobs/${jobId}`,
      { method: "GET" },
      { outcomeUnknown: false },
    );
    if (!response.ok) await assertResponse(response, onUnauthorized);
    const responseOperationId =
      response.headers.get(OPERATION_ID_HEADER) || "";
    const responseOperationState =
      response.headers.get(OPERATION_STATE_HEADER) || "";
    if (!isRecord(payload)) {
      throw new RequestError("Invalid job status response", {
        code: "invalid_job_status",
        kind: "protocol",
      });
    }
    const payloadState = payload.state;
    const hasHttpStatus = payload.http_status !== undefined;
    const httpStatus = normalizeHttpStatus(payload.http_status);
    if (
      payload.job_id !== jobId ||
      responseOperationId !== jobId ||
      !isJobState(payloadState) ||
      responseOperationState !== payloadState ||
      (payloadState === "running" && hasHttpStatus) ||
      (payloadState === "succeeded" && (httpStatus < 200 || httpStatus > 299)) ||
      (payloadState === "failed" && (httpStatus < 400 || httpStatus > 599)) ||
      (
        payloadState === "unknown" &&
        hasHttpStatus &&
        (httpStatus < 400 || httpStatus > 599)
      )
    ) {
      throw new RequestError("Invalid job status response", {
        code: "invalid_job_status",
        kind: "protocol",
      });
    }
    return /** @type {JobQueryResult} */ (Object.freeze({
      kind: payloadState,
      jobId,
      state: payloadState,
      status: httpStatus,
      code: typeof payload.code === "string" ? payload.code : "",
      message: jobStatusMessage(payload),
      authenticationFailed: false,
    }));
  } catch (statusError) {
    if (isAuthenticationError(statusError)) {
      return Object.freeze({
        kind: "authentication",
        jobId,
        state: "unknown",
        status: 0,
        code: "authentication_required",
        message: AUTH_REQUIRED_MESSAGE,
        authenticationFailed: true,
      });
    }
    return Object.freeze({
      kind: "unavailable",
      jobId,
      state: "unknown",
      status: 0,
      code: "job_status_unavailable",
      message:
        `${RESULT_UNKNOWN_MESSAGE} ` +
        `The one-time status check failed: ${requestErrorMessage(statusError)}`,
      authenticationFailed: false,
    });
  }
}

/** @param {unknown} error @returns {string} */
function jobIdForUnknownMutation(error) {
  return error instanceof RequestError &&
      error.outcomeUnknown &&
      OPERATION_ID_PATTERN.test(error.operationId)
    ? error.operationId
    : "";
}

/**
 * @typedef {Object} UploadQueryResult
 * @property {string} operationId
 * @property {string} state
 * @property {number} status
 * @property {string} code
 * @property {string} message
 * @property {boolean} authenticationFailed
 */

/**
 * @param {unknown} error
 * @param {RequestInfo | URL} url
 * @param {number} expectedLength
 * @param {(() => void) | undefined} [onUnauthorized]
 * @returns {Promise<Readonly<UploadQueryResult> | null>}
 */
export async function queryUnknownUpload(
  error,
  url,
  expectedLength,
  onUnauthorized,
) {
  if (
    !(error instanceof RequestError) ||
    !error.outcomeUnknown ||
    !OPERATION_ID_PATTERN.test(error.operationId)
  ) {
    return null;
  }

  try {
    const response = await requestHead(url, {
      headers: { [UPLOAD_ID_HEADER]: error.operationId },
    });
    const classification = classifyUploadResponse({
      phase: "checkpoint",
      status: response.status,
      headers: response.headers,
      expectedUploadId: error.operationId,
      expectedLength,
    });
    if (["authentication", "csrf"].includes(classification.kind)) {
      await assertResponse(response, onUnauthorized);
    }
    if (classification.kind === "invalid") {
      await assertResponse(response, onUnauthorized);
      throw new RequestError("Invalid upload status response", {
        code: "invalid_upload_status",
        kind: "protocol",
      });
    }
    return Object.freeze({
      operationId: error.operationId,
      state: classification.kind,
      status: response.status,
      code: "",
      message: uploadStatusMessage(classification.kind),
      authenticationFailed: false,
    });
  } catch (statusError) {
    if (isAuthenticationError(statusError)) {
      return Object.freeze({
        operationId: error.operationId,
        state: "unknown",
        status: 0,
        code: "authentication_required",
        message: AUTH_REQUIRED_MESSAGE,
        authenticationFailed: true,
      });
    }
    return Object.freeze({
      operationId: error.operationId,
      state: "unknown",
      status: 0,
      code: "upload_status_unavailable",
      message:
        `${RESULT_UNKNOWN_MESSAGE} ` +
        `The one-time upload status check failed: ${requestErrorMessage(statusError)}`,
      authenticationFailed: false,
    });
  }
}

/** @param {Response} response @returns {Promise<string>} */
export async function requestTextError(response) {
  try {
    const buffered = await bufferResponse(response, "GET", {
      limit: ERROR_RESPONSE_BODY_LIMIT,
    });
    return await buffered.text();
  } catch {
    return "";
  }
}

/** @param {string} value @returns {string} */
function normalizeErrorMessage(value) {
  const message = value.trim();
  if (!message || message.length > ERROR_MESSAGE_LIMIT) return "";
  return message;
}

/** @param {string} state @returns {string} */
function uploadStatusMessage(state) {
  if (state === "committed") {
    return "The server confirms that the upload was committed.";
  }
  if (state === "rejected") {
    return "The server confirms that the upload was rejected.";
  }
  if (state === "not-seen") {
    return "The server did not record this upload ID.";
  }
  return (
    "The server reports that the upload is still running. " +
    "Refresh the folder before trying again."
  );
}

/** @param {Record<string, unknown>} payload @returns {string} */
function jobStatusMessage(payload) {
  if (payload.state === "succeeded") {
    return "The server confirms that the operation succeeded.";
  }
  if (payload.state === "failed") {
    const detail = normalizeErrorMessageValue(payload.detail);
    return detail
      ? `The server confirms that the operation failed: ${detail}`
      : "The server confirms that the operation failed.";
  }
  if (payload.state === "unknown") {
    return (
      "The server could not prove the final operation outcome. " +
      "Refresh the folder to inspect the target before trying again."
    );
  }
  return (
    "The server reports that the operation is still running. " +
    "Refresh the folder before trying again."
  );
}

/** @param {unknown} error @returns {string} */
function requestErrorMessage(error) {
  if (error instanceof Error && error.message) return error.message;
  return "status unavailable";
}
