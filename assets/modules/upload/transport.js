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
  let oversized = false;

  const rejectOversizedResponse = () => {
    if (oversized) return;
    oversized = true;
    options.onOversizedResponse();
    request.abort();
  };

  request.upload.addEventListener("progress", options.onProgress);
  request.upload.addEventListener("load", options.onBodySent);
  request.addEventListener("readystatechange", () => {
    if (request.readyState !== XMLHttpRequest.HEADERS_RECEIVED) return;
    const declaredLength = parseUnsignedHeader(
      request.getResponseHeader("Content-Length"),
    );
    if (declaredLength !== null && declaredLength > responseLimit) {
      rejectOversizedResponse();
    }
  });
  request.addEventListener("progress", event => {
    if (event.loaded > responseLimit) rejectOversizedResponse();
  });
  request.addEventListener("load", () => {
    if (oversized || responseTextExceedsLimit(
      request.responseText,
      responseLimit,
    )) {
      rejectOversizedResponse();
      return;
    }
    options.onResponse(request);
  });
  request.addEventListener("error", options.onNetworkError);
  request.addEventListener("abort", options.onAbort);
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
