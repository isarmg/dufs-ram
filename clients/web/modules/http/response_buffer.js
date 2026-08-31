import { parseUnsignedHeader } from "./headers.js";

export const ERROR_RESPONSE_BODY_LIMIT = 16 * 1024;
export const SUCCESS_RESPONSE_BODY_LIMIT = 16 * 1024 * 1024;

/**
 * @typedef {{
 *   limit?: number,
 *   outcomeUnknown?: boolean,
 *   operationId?: string,
 * }} ResponseBufferOptions
 */

/**
 * @typedef {{
 *   status: number,
 *   code: string,
 *   kind: string,
 *   outcomeUnknown: boolean,
 *   operationId: string,
 *   operationState: string,
 * }} ResponseBufferErrorOptions
 */

/** @typedef {(message: string, options: ResponseBufferErrorOptions) => Error} ResponseErrorFactory */

/**
 * @param {Response} response
 * @param {string} [method]
 * @param {ResponseBufferOptions} [options]
 * @param {ResponseErrorFactory} [createError]
 * @returns {Promise<Response>}
 */
export async function bufferResponse(
  response,
  method = "GET",
  options = {},
  createError = defaultErrorFactory,
) {
  if (
    String(method).toUpperCase() === "HEAD" ||
    [204, 205, 304].includes(response.status)
  ) {
    return response;
  }
  const limit = options.limit ?? (response.ok
    ? SUCCESS_RESPONSE_BODY_LIMIT
    : ERROR_RESPONSE_BODY_LIMIT);
  if (!Number.isSafeInteger(limit) || limit <= 0) {
    throw new TypeError("Response body limit must be a positive integer");
  }
  const declaredLength = parseUnsignedHeader(
    response.headers.get("content-length"),
  );
  if (declaredLength !== null && declaredLength > limit) {
    await cancelResponseBody(response.body);
    throw responseBodyTooLarge(response, options, createError);
  }
  if (!response.body) return response;

  const reader = response.body.getReader();
  /** @type {Uint8Array[]} */
  const chunks = [];
  let received = 0;
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      if (!(value instanceof Uint8Array)) {
        await cancelReader(reader);
        throw createError("Invalid response body stream", {
          status: response.status,
          code: "invalid_response_body",
          kind: "protocol",
          outcomeUnknown: Boolean(options.outcomeUnknown),
          operationId: options.operationId || "",
          operationState: options.outcomeUnknown ? "unknown" : "",
        });
      }
      if (value.byteLength > limit - received) {
        await cancelReader(reader);
        throw responseBodyTooLarge(response, options, createError);
      }
      chunks.push(value);
      received += value.byteLength;
    }
  } finally {
    reader.releaseLock();
  }

  return new Response(replayBufferedChunks(chunks), {
    status: response.status,
    statusText: response.statusText,
    headers: response.headers,
  });
}

/** @param {Uint8Array[]} chunks @returns {ReadableStream<Uint8Array>} */
function replayBufferedChunks(chunks) {
  let index = 0;
  return new ReadableStream({
    pull(controller) {
      if (index >= chunks.length) {
        chunks.length = 0;
        controller.close();
        return;
      }
      controller.enqueue(chunks[index++]);
    },
    cancel() {
      chunks.length = 0;
    },
  });
}

/**
 * @param {Response} response
 * @param {ResponseBufferOptions} options
 * @param {ResponseErrorFactory} createError
 */
function responseBodyTooLarge(response, options, createError) {
  return createError("The server response exceeded the allowed size", {
    status: response.status,
    code: "response_body_too_large",
    kind: "protocol",
    outcomeUnknown: Boolean(options.outcomeUnknown),
    operationId: options.operationId || "",
    operationState: options.outcomeUnknown ? "unknown" : "",
  });
}

/**
 * @param {string} message
 * @param {ResponseBufferErrorOptions} options
 */
function defaultErrorFactory(message, options) {
  return Object.assign(new Error(message), options);
}

/** @param {ReadableStream<Uint8Array> | null} body */
async function cancelResponseBody(body) {
  try {
    await body?.cancel();
  } catch {
    // Cancellation is best-effort after the response has already been rejected.
  }
}

/** @param {ReadableStreamDefaultReader<Uint8Array>} reader */
async function cancelReader(reader) {
  try {
    await reader.cancel();
  } catch {
    // Cancellation is best-effort after the response has already been rejected.
  }
}
