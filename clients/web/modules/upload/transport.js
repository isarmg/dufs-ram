import { parseUnsignedHeader } from "../http/headers.js";

/**
 * @typedef {{
 *   responseLimit: number,
 *   onProgress: (event: ProgressEvent) => void,
 *   onBodySent: (event: ProgressEvent) => void,
 *   onResponse: (request: XMLHttpRequest) => void,
 *   onNetworkError: (event: ProgressEvent) => void,
 *   onAbort: (event: ProgressEvent) => void,
 *   onOversizedResponse: () => void,
 * }} UploadRequestOptions
 */

/** Configure an XHR upload without dispatching it yet. */
/** @param {UploadRequestOptions} options */
export function createUploadRequest(options) {
  const request = new XMLHttpRequest();
  const responseLimit = options.responseLimit;
  let settled = false;

  const rejectOversizedResponse = () => {
    if (settled) return;
    settled = true;
    options.onOversizedResponse();
    request.abort();
  };

  request.upload.addEventListener("progress", options.onProgress);
  request.upload.addEventListener("load", options.onBodySent);
  request.addEventListener("readystatechange", () => {
    if (
      settled ||
      request.readyState !== XMLHttpRequest.HEADERS_RECEIVED
    ) {
      return;
    }
    if (request.status === 401) {
      // Authentication is complete at the response-header boundary. Mark the
      // transport settled before invoking application code because its reload
      // callback can synchronously cause an abort event.
      settled = true;
      try {
        options.onResponse(request);
      } finally {
        request.abort();
      }
      return;
    }
    const declaredLength = parseUnsignedHeader(
      request.getResponseHeader("Content-Length"),
    );
    if (declaredLength !== null && declaredLength > responseLimit) {
      rejectOversizedResponse();
    }
  });
  request.addEventListener("progress", event => {
    if (settled) return;
    if (event.loaded > responseLimit) rejectOversizedResponse();
  });
  request.addEventListener("load", () => {
    if (settled) return;
    if (responseTextExceedsLimit(
      request.responseText,
      responseLimit,
    )) {
      rejectOversizedResponse();
      return;
    }
    settled = true;
    options.onResponse(request);
  });
  request.addEventListener("error", event => {
    if (settled) return;
    settled = true;
    options.onNetworkError(event);
  });
  request.addEventListener("abort", event => {
    if (settled) return;
    settled = true;
    options.onAbort(event);
  });
  return request;
}

/** Open, populate and dispatch a previously configured upload request. */
/**
 * @param {XMLHttpRequest} request
 * @param {{
 *   method: string,
 *   url: string,
 *   headers: Record<string, string>,
 *   body: Document | XMLHttpRequestBodyInit | null,
 * }} options
 */
export function dispatchUploadRequest(request, options) {
  request.open(options.method, options.url);
  for (const [name, value] of Object.entries(options.headers)) {
    request.setRequestHeader(name, value);
  }
  request.send(options.body);
}

/** @param {unknown} value @param {number} limit */
function responseTextExceedsLimit(value, limit) {
  if (typeof value !== "string") return false;
  if (value.length > limit) return true;
  return new TextEncoder().encode(value).byteLength > limit;
}
