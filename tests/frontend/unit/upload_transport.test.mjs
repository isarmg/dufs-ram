import assert from "node:assert/strict";
import test from "node:test";

class FakeEventTarget {
  constructor() {
    this.listeners = new Map();
  }

  addEventListener(name, listener) {
    const listeners = this.listeners.get(name) || [];
    listeners.push(listener);
    this.listeners.set(name, listeners);
  }

  emit(name, event = {}) {
    for (const listener of this.listeners.get(name) || []) {
      listener.call(this, event);
    }
  }
}

class FakeXMLHttpRequest extends FakeEventTarget {
  static HEADERS_RECEIVED = 2;

  constructor() {
    super();
    this.upload = new FakeEventTarget();
    this.readyState = 0;
    this.status = 0;
    this.responseText = "";
    this.headers = new Map();
    this.aborted = false;
  }

  getResponseHeader(name) {
    return this.headers.get(name.toLowerCase()) ?? null;
  }

  receiveHeaders(status, headers = {}) {
    this.status = status;
    this.headers = new Map(Object.entries(headers).map(
      ([name, value]) => [name.toLowerCase(), String(value)],
    ));
    this.readyState = FakeXMLHttpRequest.HEADERS_RECEIVED;
    this.emit("readystatechange");
  }

  abort() {
    this.aborted = true;
    this.emit("abort", { type: "abort" });
  }
}

test("upload transport settles authentication at response headers", async () => {
  const previousXMLHttpRequest = globalThis.XMLHttpRequest;
  globalThis.XMLHttpRequest = FakeXMLHttpRequest;
  try {
    const { createUploadRequest } = await import(
      "../../../clients/web/modules/upload/transport.js"
    );
    const responseLimit = 16 * 1024;
    const cases = [
      {
        name: "declared oversized 401",
        status: 401,
        headers: { "Content-Length": responseLimit + 1 },
        afterHeaders(request) {
          request.emit("progress", { loaded: responseLimit + 1 });
          request.emit("load");
        },
      },
      {
        name: "streamed oversized 401",
        status: 401,
        headers: {},
        afterHeaders(request) {
          request.emit("progress", { loaded: responseLimit + 1 });
          request.responseText = "x".repeat(responseLimit + 1);
          request.emit("load");
        },
      },
      {
        name: "stalled 401",
        status: 401,
        headers: {},
        afterHeaders() {},
      },
    ];

    for (const entry of cases) {
      const calls = {
        response: 0,
        networkError: 0,
        abort: 0,
        oversized: 0,
      };
      const request = createUploadRequest({
        responseLimit,
        onProgress() {},
        onBodySent() {},
        onResponse() {
          calls.response++;
        },
        onNetworkError() {
          calls.networkError++;
        },
        onAbort() {
          calls.abort++;
        },
        onOversizedResponse() {
          calls.oversized++;
        },
      });

      request.receiveHeaders(entry.status, entry.headers);
      assert.equal(request.aborted, true, `${entry.name} body was not aborted`);
      assert.deepEqual(calls, {
        response: 1,
        networkError: 0,
        abort: 0,
        oversized: 0,
      });

      entry.afterHeaders(request);
      request.emit("error", { type: "error" });
      request.emit("abort", { type: "abort" });
      assert.deepEqual(calls, {
        response: 1,
        networkError: 0,
        abort: 0,
        oversized: 0,
      }, `${entry.name} produced a second terminal callback`);
    }
  } finally {
    if (previousXMLHttpRequest === undefined) {
      delete globalThis.XMLHttpRequest;
    } else {
      globalThis.XMLHttpRequest = previousXMLHttpRequest;
    }
  }
});
