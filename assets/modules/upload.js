import {
  createElement,
  createIcon,
  errorMessage,
  formatFileSize,
} from "./dom.js";
import {
  AUTH_ERROR_HEADER,
  AUTH_REQUIRED_MESSAGE,
  CSRF_HEADER,
  PAGE_EXPIRED_MESSAGE,
  authFailureMessage,
  isCsrfAuthFailure,
} from "./api.js";
import { childUrl } from "./path.js";

const MAX_CONCURRENT_UPLOADS = 1;
const IDLE_TIMEOUT_MS = 2 * 60 * 1000;
const TOTAL_TIMEOUT_MS = 24 * 60 * 60 * 1000;
const STATUS_TIMEOUT_MS = 30 * 1000;

export function createUploadManager(options) {
  const {
    data,
    uploadersTable,
    queueMessage,
    emptyFolder,
    onUnauthorized,
  } = options;
  const queue = [];
  const failed = new Map();
  let running = 0;
  let nextIndex = 0;
  let queueState = "running";

  const beforeUnload = event => {
    if (queueState === "running" && (queue.length > 0 || running > 0)) {
      event.preventDefault();
      event.returnValue = "";
      return "";
    }
  };
  window.addEventListener("beforeunload", beforeUnload);

  function pauseForAuthentication() {
    if (queueState === "paused-auth") return;
    queueState = "paused-auth";
    window.removeEventListener("beforeunload", beforeUnload);
    queueMessage.textContent = PAGE_EXPIRED_MESSAGE;
    queueMessage.classList.remove("hidden");
  }

  function runQueue() {
    if (queueState !== "running" || running >= MAX_CONCURRENT_UPLOADS) return;
    const uploader = queue.shift();
    if (!uploader) return;
    running++;
    uploader.runningAccounted = true;
    uploader.start();
  }

  class Uploader {
    constructor(file, pathParts) {
      this.index = nextIndex++;
      this.file = file;
      this.name = [...pathParts, file.name].join("/");
      this.url = childUrl(this.name);
      this.uploadId = crypto.randomUUID();
      this.uploadOffset = 0;
      this.uploaded = 0;
      this.lastUpdate = 0;
      this.state = "new";
      this.retryPending = false;
      this.runningAccounted = false;
      this.abortController = null;
      this.abortReason = "";
      this.idleTimer = null;
      this.totalTimer = null;
      this.statusCell = null;
      this.speedNode = createElement("span", {
        className: "upload-speed",
      });
      this.progressNode = createElement("span", {
        className: "upload-progress",
      });
      this.cancelButton = createElement("button", {
        className: "upload-cancel",
        text: "取消",
        attributes: {
          type: "button",
          "aria-label": `取消上传 ${this.name}`,
        },
      });
      this.cancelButton.addEventListener("click", () => this.cancel());
    }

    enqueue() {
      const row = createElement("tr", {
        className: "uploader",
        attributes: { id: `upload${this.index}` },
      });
      const iconCell = createElement("td", { className: "path cell-icon" });
      iconCell.append(createIcon("file"));
      const nameCell = createElement("td", { className: "path cell-name" });
      nameCell.append(createElement("a", {
        text: this.name,
        attributes: { href: this.url },
      }));
      this.statusCell = createElement("td", {
        className: "cell-status upload-status",
        text: "等待",
        attributes: {
          id: `uploadStatus${this.index}`,
          role: "status",
          "aria-live": "polite",
          "aria-label": `${this.name}：等待上传`,
        },
      });
      row.append(iconCell, nameCell, this.statusCell);
      (uploadersTable.tBodies[0] || uploadersTable.createTBody()).append(row);
      uploadersTable.classList.remove("hidden");
      emptyFolder.classList.add("hidden");
      this.state = "queued";
      queue.push(this);
      runQueue();
    }

    start() {
      this.state = "running";
      if (this.retryPending) {
        this.retryPending = false;
        this.queryCheckpoint();
      } else {
        this.sendBody();
      }
    }

    sendBody() {
      this.uploaded = 0;
      this.lastUpdate = Date.now();
      const request = new XMLHttpRequest();
      this.startTimeouts(() => request.abort());
      this.renderProgress("等待数据", "0% --:--:--");
      request.upload.addEventListener("progress", event => this.onProgress(event));
      request.addEventListener("load", () => {
        this.clearTimeouts();
        if (request.status >= 200 && request.status < 300) {
          this.complete();
          return;
        }
        const authError = request.getResponseHeader(AUTH_ERROR_HEADER);
        const authMessage = authFailureMessage(request.status, authError);
        const csrfFailure = isCsrfAuthFailure(request.status, authError);
        if (request.status === 401) {
          pauseForAuthentication();
          onUnauthorized();
        }
        if (csrfFailure) pauseForAuthentication();
        this.fail(
          authMessage || `上传失败（HTTP ${request.status}）`,
          request.status !== 401 && !csrfFailure,
        );
      });
      request.addEventListener("error", () => {
        this.clearTimeouts();
        this.fail("网络连接中断");
      });
      request.addEventListener("abort", () => {
        const reason = this.abortReason || "上传已取消";
        this.clearTimeouts();
        this.fail(reason);
      });

      try {
        if (this.uploadOffset > 0) {
          request.open("PATCH", this.url);
        } else {
          request.open("PUT", this.url);
        }
        request.setRequestHeader(CSRF_HEADER, data.csrf_token);
        request.setRequestHeader("X-Dufs-Upload-Id", this.uploadId);
        request.setRequestHeader(
          "X-Dufs-Upload-Length",
          String(this.file.size),
        );
        if (this.uploadOffset > 0) {
          request.setRequestHeader(
            "X-Dufs-Upload-Offset",
            String(this.uploadOffset),
          );
          request.send(this.file.slice(this.uploadOffset));
        } else {
          request.send(this.file);
        }
      } catch (error) {
        this.clearTimeouts();
        this.fail(errorMessage(error));
      }
    }

    retry() {
      if (queueState !== "running") return;
      if (!failed.delete(this.index)) return;
      this.state = "queued";
      this.retryPending = true;
      this.statusCell.textContent = "等待重试";
      this.statusCell.setAttribute(
        "aria-label",
        `${this.name}：等待重试`,
      );
      queue.push(this);
      runQueue();
    }

    async queryCheckpoint() {
      const controller = new AbortController();
      this.abortController = controller;
      this.abortReason = "";
      this.renderCheckpointStatus();
      const timer = window.setTimeout(() => {
        this.abortReason = "检查续传状态超时";
        controller.abort();
      }, STATUS_TIMEOUT_MS);
      try {
        const response = await fetch(this.url, {
          method: "HEAD",
          signal: controller.signal,
          headers: { "X-Dufs-Upload-Id": this.uploadId },
        });
        if (response.status === 401) {
          pauseForAuthentication();
          onUnauthorized();
          this.fail(AUTH_REQUIRED_MESSAGE, false);
          return;
        }
        if (isCsrfAuthFailure(
          response.status,
          response.headers.get(AUTH_ERROR_HEADER),
        )) {
          pauseForAuthentication();
          this.fail(PAGE_EXPIRED_MESSAGE, false);
          return;
        }
        if (response.status === 404) {
          this.restartSession();
          this.sendBody();
          return;
        }
        if (response.status !== 200) {
          this.fail(`检查续传状态失败（HTTP ${response.status}）`);
          return;
        }
        const totalLength = Number(response.headers.get("x-dufs-upload-length"));
        const offset = Number(response.headers.get("x-dufs-upload-offset"));
        if (
          !Number.isSafeInteger(totalLength) ||
          totalLength !== this.file.size ||
          !Number.isSafeInteger(offset) ||
          offset < 0 ||
          offset > this.file.size
        ) {
          this.restartSession();
        } else {
          this.uploadOffset = offset;
        }
        this.sendBody();
      } catch (error) {
        this.fail(
          controller.signal.aborted
            ? this.abortReason || "检查续传状态已取消"
            : errorMessage(error),
        );
      } finally {
        window.clearTimeout(timer);
        if (this.abortController === controller) this.abortController = null;
      }
    }

    restartSession() {
      this.uploadId = crypto.randomUUID();
      this.uploadOffset = 0;
    }

    onProgress(event) {
      this.resetIdleTimeout();
      const now = Date.now();
      const elapsed = now - this.lastUpdate;
      if (elapsed < 300) return;
      const speed = (event.loaded - this.uploaded) / elapsed * 1000;
      const [speedValue, speedUnit] = formatFileSize(speed);
      const progress = this.file.size === 0
        ? "100%"
        : formatPercent(
          ((event.loaded + this.uploadOffset) / this.file.size) * 100,
        );
      const duration = speed > 0
        ? formatDuration((event.total - event.loaded) / speed)
        : "--:--:--";
      this.renderProgress(`${speedValue} ${speedUnit}/s`, `${progress} ${duration}`);
      this.uploaded = event.loaded;
      this.lastUpdate = now;
    }

    renderProgress(speedText, progressText) {
      if (this.state !== "running") return;
      if (!this.statusCell.contains(this.cancelButton)) {
        this.statusCell.replaceChildren(
          this.speedNode,
          this.progressNode,
          this.cancelButton,
        );
      }
      this.speedNode.textContent = speedText;
      this.progressNode.textContent = progressText;
      this.statusCell.setAttribute(
        "aria-label",
        `${this.name}：已上传 ${progressText}，速度 ${speedText}`,
      );
    }

    renderCheckpointStatus() {
      this.statusCell.replaceChildren(
        createElement("span", { text: "正在检查续传状态…" }),
        this.cancelButton,
      );
      this.statusCell.setAttribute(
        "aria-label",
        `${this.name}：正在检查续传状态`,
      );
    }

    complete() {
      if (this.state !== "running") return;
      this.clearTimeouts();
      this.state = "completed";
      this.statusCell.replaceChildren(createElement("span", {
        text: "✓",
        attributes: { "aria-hidden": "true" },
      }));
      this.statusCell.setAttribute("aria-label", `${this.name}：上传成功`);
      failed.delete(this.index);
      this.finishRunning();
    }

    fail(reason = "", retryable = true) {
      if (this.state !== "running") return;
      this.clearTimeouts();
      this.state = "failed";
      this.statusCell.replaceChildren(createElement("span", {
        className: "upload-failure",
        text: `✗ ${reason || "上传失败"}`,
        attributes: {
          title: reason || "上传失败",
        },
      }));
      this.statusCell.setAttribute(
        "aria-label",
        `${this.name}：${reason || "上传失败"}`,
      );
      if (retryable) {
        const retry = createElement("button", {
          className: "retry-btn",
          text: "↻",
          attributes: {
            type: "button",
            title: "重试上传",
            "aria-label": `重试上传 ${this.name}`,
          },
        });
        retry.addEventListener("click", () => this.retry());
        this.statusCell.append(retry);
        failed.set(this.index, this);
      } else {
        failed.delete(this.index);
      }
      this.finishRunning();
    }

    cancel() {
      if (this.state !== "running" || !this.abortController) return;
      this.abortReason = "上传已取消";
      this.abortController.abort();
    }

    startTimeouts(abortRequest) {
      this.clearTimeouts();
      const controller = new AbortController();
      controller.signal.addEventListener("abort", abortRequest, { once: true });
      this.abortController = controller;
      this.abortReason = "";
      this.totalTimer = window.setTimeout(() => {
        this.abortReason = "上传超过最长允许时间";
        controller.abort();
      }, TOTAL_TIMEOUT_MS);
      this.resetIdleTimeout();
    }

    resetIdleTimeout() {
      if (!this.abortController) return;
      window.clearTimeout(this.idleTimer);
      const controller = this.abortController;
      this.idleTimer = window.setTimeout(() => {
        if (this.abortController !== controller) return;
        this.abortReason = "上传长时间没有进展";
        controller.abort();
      }, IDLE_TIMEOUT_MS);
    }

    clearTimeouts() {
      window.clearTimeout(this.idleTimer);
      window.clearTimeout(this.totalTimer);
      this.idleTimer = null;
      this.totalTimer = null;
      this.abortController = null;
      this.abortReason = "";
    }

    finishRunning() {
      if (!this.runningAccounted) return;
      this.runningAccounted = false;
      running = Math.max(0, running - 1);
      runQueue();
    }
  }

  return Object.freeze({
    addFiles(files) {
      for (const file of files) {
        const relativePath = file.webkitRelativePath || file.name;
        const pathParts = relativePath.split("/").filter(Boolean);
        pathParts.pop();
        new Uploader(file, pathParts).enqueue();
      }
    },
    isBusy() {
      return running > 0 || queue.length > 0;
    },
  });
}

function formatDuration(seconds) {
  if (!Number.isFinite(seconds) || seconds < 0) return "--:--:--";
  seconds = Math.ceil(seconds);
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.floor((seconds - hours * 3600) / 60);
  const remaining = seconds - hours * 3600 - minutes * 60;
  return [hours, minutes, remaining]
    .map(value => String(value).padStart(2, "0"))
    .join(":");
}

function formatPercent(percent) {
  return `${percent > 10 ? percent.toFixed(1) : percent.toFixed(2)}%`;
}
