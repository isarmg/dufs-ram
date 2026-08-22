import { createElement, errorMessage, formatFileSize } from "../shared/dom.js";
import { MUTATION_EFFECT } from "../shared/mutation_effect.js";
import {
  AUTH_REQUIRED_MESSAGE,
  CSRF_HEADER,
  ERROR_RESPONSE_BODY_LIMIT,
  PAGE_EXPIRED_MESSAGE,
  RESULT_UNKNOWN_MESSAGE,
  assertDiscardUploadResponse,
  assertResponse,
  isAuthenticationError,
  isRequestErrorCode,
  parseErrorPayload,
  requestHead,
  requestJson,
  requestNoContent,
} from "../http/client.js";
import {
  childUrl,
  logicalChildPath,
} from "../shared/path.js";
import {
  TARGET_REVISION_HEADER,
  UPLOAD_ID_HEADER,
  UPLOAD_LENGTH_HEADER,
  UPLOAD_OFFSET_HEADER,
  UPLOAD_OVERWRITE_HEADER,
  classifyUploadResponse,
  parseTargetReplaceable,
  parseTargetRevision,
} from "./protocol.js";
import { parseUploadPreflight } from "./preflight.js";
import { createBoundedHistory, createUploadQueue } from "./queue.js";
import {
  UPLOAD_BATCH_PATH_BYTES_LIMIT,
  prepareUploadSelection,
} from "./selection.js";
import {
  createUploadRequest,
  dispatchUploadRequest,
} from "./transport.js";
import {
  createUploadView,
  renderCancelled,
  renderCheckpoint,
  renderCleanup,
  renderComplete,
  renderFailure,
  renderProgress,
  renderSkipped,
  renderSubmitting,
  renderUnknown,
  renderWaitingForOverwrite,
  renderWaiting,
} from "./view.js";

const DEFAULT_MAX_CONCURRENT_UPLOADS = 1;
const MAX_CLIENT_UPLOAD_CONCURRENCY = 8;
const ENQUEUE_BATCH_SIZE = 50;
const IDLE_TIMEOUT_MS = 2 * 60 * 1000;
const TOTAL_TIMEOUT_MS = 24 * 60 * 60 * 1000;
const STATUS_TIMEOUT_MS = 30 * 1000;
const COMMIT_TIMEOUT_MS = 5 * 60 * 1000;
const MAX_TIMER_DELAY_MS = 2_147_483_647;
export const UPLOAD_PENDING_ROW_LIMIT = 512;
export const UPLOAD_TERMINAL_ROW_LIMIT = 200;
const UPLOAD_RECOVERY_LABELS = Object.freeze({
  retry: "Retry upload",
  query_upload: "Check upload status",
});
const UNKNOWN_UPLOAD_RESULT_MESSAGE =
  `Upload data was sent, but the server did not confirm the result. ${RESULT_UNKNOWN_MESSAGE}`;

/** @typedef {"new" | "queued" | "running" | "completed" | "failed" | "unknown" | "cancelled"} UploadLifecycleState */
/** @typedef {"new" | "transferring" | "checking" | "submitting" | "awaiting-confirmation" | "completed" | "failed" | "unknown" | "cancelled"} UploadPhase */
/** @typedef {"" | "retry" | "query_upload"} UploadRecoveryAction */
/** @typedef {"confirm" | "alternate" | "cancel"} DialogChoice */

/**
 * @typedef {{
 *   href: string,
 *   dir_exists: boolean,
 *   user: string,
 *   csrf_token: string,
 *   max_concurrent_uploads?: number,
 * }} IndexData
 */

/**
 * @typedef {{
 *   row: HTMLTableRowElement,
 *   statusCell: HTMLTableCellElement,
 *   speedNode: HTMLSpanElement,
 *   progressNode: HTMLSpanElement,
 *   liveNode: HTMLSpanElement,
 *   cancelButton: HTMLButtonElement,
 * }} UploadView
 */

/** @typedef {import("./selection.js").UploadSelectionEntry} UploadSelectionEntry */

/**
 * @typedef {{ file: File, name: string, revision: string | null }} QueuedUploadEntry
 */

/**
 * @typedef {{
 *   data: IndexData,
 *   dialogs: {
 *     showMessage: (options: {
 *       title: string,
 *       message?: string,
 *       returnFocus?: Element | null,
 *     }) => Promise<undefined>,
 *     chooseAction: (options: {
 *       title: string,
 *       message?: string,
 *       confirmText?: string,
 *       alternateText?: string,
 *       cancelText?: string,
 *       danger?: boolean,
 *       returnFocus?: Element | null,
 *     }) => Promise<DialogChoice>,
 *   },
 *   uploadersTable: HTMLTableElement,
 *   queueMessage: HTMLElement,
 *   historyStatus: HTMLElement,
 *   emptyFolder: HTMLElement,
 *   onMutation: (effect: (typeof MUTATION_EFFECT)[keyof typeof MUTATION_EFFECT]) => void,
 *   onUnauthorized: () => void,
 *   maxConcurrentUploads?: number,
 *   maxTerminalRows?: number,
 * }} UploadManagerOptions
 */

/**
 * @typedef {{
 *   row: HTMLTableRowElement,
 *   name: string,
 *   dispose: (() => void) | null,
 * }} TerminalUploadRow
 */

/** @param {UploadManagerOptions} options */
export function createUploadManager(options) {
  const {
    data,
    dialogs,
    uploadersTable,
    queueMessage,
    historyStatus,
    emptyFolder,
    onMutation,
    onUnauthorized,
  } = options;
  const maxConcurrentUploads = normalizeConcurrency(
    options.maxConcurrentUploads ?? data.max_concurrent_uploads,
  );
  const maxTerminalRows = normalizeTerminalRowLimit(options.maxTerminalRows);
  /** @type {ReturnType<typeof createUploadQueue>} */
  const queue = createUploadQueue();
  /** @type {Map<number, Uploader>} */
  const failed = new Map();
  /** @type {Set<number>} */
  const unresolvedUnknown = new Set();
  /** @type {Set<string>} */
  const knownTargets = new Set();
  let running = 0;
  let pendingRows = 0;
  let nextIndex = 0;
  let nextBatchId = 0;
  /** @type {Map<number, {
   *   members: Set<Uploader>,
   *   enqueueComplete: boolean,
   *   cancelRequested: boolean,
   *   remainingEntries: number,
   * }>} */
  const batches = new Map();
  let cancellingBatch = false;
  /** @type {"running" | "paused-auth" | "paused-unknown"} */
  let queueState = "running";
  /** @type {Promise<void>} */
  let enqueueTail = Promise.resolve();
  let restoreHistoryFocusAfterRender = false;
  const terminalHistory = createBoundedHistory(
    maxTerminalRows,
    /** @param {TerminalUploadRow} entry */ entry => {
      const restoreFocus = entry.row.contains(document.activeElement);
      const adjacentNameLink = restoreFocus
        ? adjacentUploadNameLink(entry.row)
        : null;
      entry.dispose?.();
      entry.row.remove();
      if (!restoreFocus) return;
      if (adjacentNameLink?.isConnected) {
        adjacentNameLink.focus({ preventScroll: true });
      } else {
        restoreHistoryFocusAfterRender = true;
      }
    },
  );

  /** @param {BeforeUnloadEvent} event */
  const beforeUnload = event => {
    if (queueState === "running" && (queue.size > 0 || running > 0)) {
      event.preventDefault();
      event.returnValue = "";
      return "";
    }
  };
  window.addEventListener("beforeunload", beforeUnload);

  /** @param {Uploader} uploader @param {(() => void) | null} [dispose] */
  function retainTerminalRow(uploader, dispose = null) {
    if (uploader.pendingAccounted) {
      uploader.pendingAccounted = false;
      pendingRows = Math.max(0, pendingRows - 1);
    }
    if (uploader.terminalHistoryEntry) {
      terminalHistory.remove(uploader.terminalHistoryEntry);
    }
    const entry = {
      row: uploader.view.row,
      name: uploader.name,
      dispose,
    };
    knownTargets.delete(uploader.name);
    uploader.terminalHistoryEntry = entry;
    const batch = batches.get(uploader.batchId);
    batch?.members.delete(uploader);
    if (batch?.enqueueComplete && batch.members.size === 0) {
      batches.delete(uploader.batchId);
    }
    terminalHistory.add(entry);
    renderHistoryStatus();
  }

  function renderHistoryStatus() {
    const hiddenCount = terminalHistory.evicted;
    historyStatus.textContent = hiddenCount > 0
      ? `${hiddenCount} older upload result${hiddenCount === 1 ? "" : "s"} hidden; ` +
        `showing the most recent ${terminalHistory.size}.`
      : "";
    historyStatus.classList.toggle("hidden", hiddenCount === 0);
    if (restoreHistoryFocusAfterRender) {
      restoreHistoryFocusAfterRender = false;
      historyStatus.tabIndex = -1;
      historyStatus.focus({ preventScroll: true });
      historyStatus.addEventListener(
        "blur",
        () => historyStatus.removeAttribute("tabindex"),
        { once: true },
      );
    }
  }

  /** @param {number} [additionalRows] */
  function hasPendingCapacity(additionalRows = 1) {
    return pendingRows + additionalRows <= UPLOAD_PENDING_ROW_LIMIT;
  }

  function showPendingLimitMessage() {
    queueMessage.textContent =
      `At most ${UPLOAD_PENDING_ROW_LIMIT} uploads may be pending at once. ` +
      "Wait for pending uploads to finish before adding or retrying more.";
    queueMessage.classList.remove("hidden");
  }

  /** @param {Uploader} uploader */
  function releaseTerminalRow(uploader) {
    if (!uploader.terminalHistoryEntry) return;
    terminalHistory.remove(uploader.terminalHistoryEntry);
    uploader.terminalHistoryEntry = null;
    renderHistoryStatus();
  }

  function pauseForAuthentication() {
    if (queueState === "paused-auth") return;
    queueState = "paused-auth";
    window.removeEventListener("beforeunload", beforeUnload);
    queueMessage.textContent = PAGE_EXPIRED_MESSAGE;
    queueMessage.classList.remove("hidden");
  }

  /** @param {Uploader} uploader */
  function pauseForUnknown(uploader) {
    unresolvedUnknown.add(uploader.index);
    if (queueState !== "paused-auth") queueState = "paused-unknown";
    window.removeEventListener("beforeunload", beforeUnload);
    queueMessage.textContent =
      `Upload result for ${uploader.name} is unknown. The remaining upload queue is paused; refresh the folder before selecting files again.`;
    queueMessage.classList.remove("hidden");
  }

  /** @param {Uploader} uploader */
  function resolveUnknown(uploader) {
    if (!unresolvedUnknown.delete(uploader.index)) return;
    if (unresolvedUnknown.size > 0 || queueState !== "paused-unknown") return;
    queueState = "running";
    window.addEventListener("beforeunload", beforeUnload);
    queueMessage.classList.add("hidden");
    runQueue();
  }

  function runQueue() {
    if (queueState !== "running" || cancellingBatch) return;
    while (running < maxConcurrentUploads) {
      const uploader = /** @type {Uploader | null} */ (queue.dequeue());
      if (!uploader) return;
      if (uploader.state !== "queued") continue;
      uploader.queueEntry = null;
      running++;
      uploader.runningAccounted = true;
      uploader.start();
    }
  }

  class Uploader {
    /**
     * @param {File} file
     * @param {string} name
     * @param {string | null} targetRevision
     * @param {number} batchId
     */
    constructor(file, name, targetRevision, batchId) {
      this.index = nextIndex++;
      this.file = file;
      this.name = name;
      this.url = childUrl(this.name);
      this.logicalPath = logicalChildPath(data.href, this.name);
      this.targetRevision = targetRevision;
      this.missingTargetRetryUsed = false;
      this.batchId = batchId;
      this.uploadId = crypto.randomUUID();
      this.uploadOffset = 0;
      this.uploaded = 0;
      this.lastUpdate = 0;
      this.lastProgressAt = 0;
      this.lastAnnouncedProgress = -1;
      /** @type {UploadLifecycleState} */
      this.state = "new";
      /** @type {UploadRecoveryAction} */
      this.pendingRecovery = "";
      /** @type {UploadRecoveryAction} */
      this.recoveryAction = "";
      this.recoveryAvailableAt = 0;
      /** @type {number | null} */
      this.recoveryTimer = null;
      this.runningAccounted = false;
      this.pendingAccounted = false;
      /** @type {{ active: boolean } | null} */
      this.queueEntry = null;
      /** @type {AbortController | null} */
      this.abortController = null;
      this.abortReason = "";
      this.abortOutcomeUnknown = false;
      /** @type {number | null} */
      this.idleTimer = null;
      /** @type {number | null} */
      this.totalTimer = null;
      /** @type {number | null} */
      this.commitTimer = null;
      /** @type {TerminalUploadRow | null} */
      this.terminalHistoryEntry = null;
      /** @type {UploadPhase} */
      this.phase = "new";
      this.uploadRequestPhase = "fresh";
      this.requestDispatched = false;
      this.view = createUploadView(
        this.index,
        this.name,
        this.url,
        () => this.cancel(),
      );
    }

    /** @param {UploadRecoveryAction} recovery */
    enqueue(recovery = "") {
      releaseTerminalRow(this);
      if (!this.pendingAccounted) {
        this.pendingAccounted = true;
        pendingRows++;
      }
      if (!this.view.row.isConnected) {
        (uploadersTable.tBodies[0] || uploadersTable.createTBody())
          .append(this.view.row);
      }
      uploadersTable.classList.remove("hidden");
      emptyFolder.classList.add("hidden");
      this.state = "queued";
      this.pendingRecovery = recovery;
      renderWaiting(this.view, this.name, Boolean(recovery));
      this.queueEntry = queue.enqueue(this);
      runQueue();
    }

    start() {
      this.state = "running";
      const recovery = this.pendingRecovery;
      this.pendingRecovery = "";
      switch (recovery) {
        case "retry":
          void this.queryCheckpoint(false, true);
          break;
        case "query_upload":
          void this.queryCheckpoint();
          break;
        default:
          this.sendBody();
      }
    }

    sendBody(forceResume = false) {
      this.uploaded = 0;
      this.lastUpdate = Date.now();
      this.lastProgressAt = this.lastUpdate;
      this.lastAnnouncedProgress = 0;
      this.phase = "transferring";
      this.requestDispatched = false;
      const request = createUploadRequest({
        responseLimit: ERROR_RESPONSE_BODY_LIMIT,
        onProgress: /** @param {ProgressEvent} event */ event =>
          this.onProgress(event),
        onBodySent: () => this.beginCommitWait(),
        onResponse: /** @param {XMLHttpRequest} response */ response =>
          this.handleUploadResponse(response),
        onNetworkError: () => this.handleNetworkError(),
        onAbort: () => this.handleAbort(),
        onOversizedResponse: () => this.handleOversizedResponse(),
      });
      this.startTimeouts(() => request.abort());
      renderProgress(
        this.view,
        this.name,
        "Waiting for data",
        "0% --:--:--",
        true,
      );

      try {
        const resuming = forceResume || this.uploadOffset > 0;
        this.uploadRequestPhase = resuming ? "resume" : "fresh";
        const commonHeaders = {
          [CSRF_HEADER]: data.csrf_token,
          [UPLOAD_ID_HEADER]: this.uploadId,
          [UPLOAD_LENGTH_HEADER]: String(this.file.size),
          [UPLOAD_OVERWRITE_HEADER]: String(this.targetRevision !== null),
          ...(this.targetRevision === null
            ? {}
            : { [TARGET_REVISION_HEADER]: this.targetRevision }),
        };
        const headers = resuming
          ? {
            ...commonHeaders,
            [UPLOAD_OFFSET_HEADER]: String(this.uploadOffset),
          }
          : commonHeaders;
        this.requestDispatched = true;
        dispatchUploadRequest(request, {
          method: resuming ? "PATCH" : "PUT",
          url: this.url,
          headers,
          body: resuming ? this.file.slice(this.uploadOffset) : this.file,
        });
      } catch (error) {
        this.requestDispatched = false;
        this.clearTimeouts();
        this.fail(errorMessage(error), "retry");
      }
    }

    /** @param {XMLHttpRequest} request */
    handleUploadResponse(request) {
      this.clearTimeouts();
      const classification = classifyUploadResponse({
        phase: this.uploadRequestPhase === "resume" ? "resume" : "fresh",
        status: request.status,
        headers: name => request.getResponseHeader(name),
        expectedUploadId: this.uploadId,
        expectedLength: this.file.size,
      });
      if (classification.kind === "committed") {
        this.complete();
        return;
      }

      if (classification.kind === "authentication") {
        pauseForAuthentication();
        onUnauthorized();
        this.fail(AUTH_REQUIRED_MESSAGE);
        return;
      }
      if (classification.kind === "csrf") {
        pauseForAuthentication();
        this.fail(PAGE_EXPIRED_MESSAGE);
        return;
      }
      const detail = parseErrorPayload(
        request.responseText,
        request.getResponseHeader("Content-Type") || "",
      );
      if (detail.status && detail.status !== request.status) {
        this.unknown(
          `Invalid error response: problem status does not match HTTP status. ${RESULT_UNKNOWN_MESSAGE}`,
          detail.recovery === "query_upload" ? "query_upload" : "",
          responseRetryAfter(request, detail.retryAfter),
        );
        return;
      }
      const targetChange = trustedUploadTargetChange(
        classification,
        detail,
        name => request.getResponseHeader(name),
        this.file.size,
      );
      if (targetChange) this.invalidateTargetChange();
      if (targetChange?.kind === "missing") {
        this.retryMissingTarget(
          classification.kind === "awaiting-confirmation",
        );
        return;
      }
      if (targetChange?.kind === "reset-stage") {
        this.retryMissingTarget(true);
        return;
      }
      if (targetChange?.kind === "exists") {
        const staged = classification.kind === "awaiting-confirmation";
        if (!targetChange.replaceable) {
          if (staged) {
            void this.discardStaged("destination cannot be replaced");
          } else {
            this.skipConflict("destination cannot be replaced");
          }
          return;
        }
        void this.resolveDestinationConflict(
          targetChange.revision,
          staged,
        );
        return;
      }
      if (classification.kind === "unknown") {
        this.unknown(
          detail.message
            ? `${detail.message}. ${RESULT_UNKNOWN_MESSAGE}`
            : UNKNOWN_UPLOAD_RESULT_MESSAGE,
          detail.recovery === "query_upload" ? "query_upload" : "",
          responseRetryAfter(request, detail.retryAfter),
        );
        return;
      }
      const knownFailureMessage = uploadFailureMessage(classification.kind);
      if (knownFailureMessage) {
        const recovery = uploadRecoveryAction(
          detail.recovery,
          classification.kind,
          classification.protocol,
        );
        this.fail(
          detail.message || knownFailureMessage,
          recovery,
          responseRetryAfter(request, detail.retryAfter),
        );
        return;
      }
      this.unknown(
        `The server returned an inconsistent upload response (HTTP ${request.status}). ${RESULT_UNKNOWN_MESSAGE}`,
        detail.recovery === "query_upload" ? "query_upload" : "",
        responseRetryAfter(request, detail.retryAfter),
      );
    }

    invalidateTargetChange() {
      if (this.state !== "running") return;
      onMutation(MUTATION_EFFECT.REFRESH_REQUIRED);
    }

    /** @param {string} revision @param {boolean} staged */
    async resolveDestinationConflict(revision, staged) {
      if (this.state !== "running") return;
      this.phase = "awaiting-confirmation";
      renderWaitingForOverwrite(this.view, this.name);
      const choice = await dialogs.chooseAction({
        title: "Upload destination changed",
        message: staged
          ? `"${this.name}" now exists or changed while its data was uploaded. The uploaded data is staged; overwrite the current destination, skip this file, or cancel the remaining queued files.`
          : `"${this.name}" now exists or changed before its data was sent. Overwrite the current destination, skip this file, or cancel the remaining queued files.`,
        confirmText: "Overwrite",
        alternateText: "Skip file",
        cancelText: "Cancel remaining",
        danger: true,
        returnFocus: this.view.row.querySelector("a"),
      });
      if (
        this.state !== "running" ||
        this.phase !== "awaiting-confirmation"
      ) {
        return;
      }
      if (choice === "confirm") {
        this.targetRevision = revision;
        this.missingTargetRetryUsed = false;
        if (staged) {
          this.publishStaged();
        } else {
          this.sendBody();
        }
        return;
      }
      if (choice === "cancel") this.cancelRemainingBatch();
      if (staged) {
        await this.discardStaged();
      } else {
        this.skipConflict();
      }
    }

    /** @param {boolean} staged */
    retryMissingTarget(staged) {
      if (staged) {
        void this.discardStaged("destination was removed", true);
        return;
      }
      if (this.missingTargetRetryUsed) {
        this.fail(
          "The upload destination kept changing; check its status before trying again",
          "query_upload",
        );
        return;
      }
      this.missingTargetRetryUsed = true;
      this.targetRevision = null;
      this.sendBody();
    }

    publishStaged() {
      if (this.targetRevision === null) {
        this.fail(
          "The staged upload has no safe target revision; discard it before retrying",
          "query_upload",
        );
        return;
      }
      this.uploaded = 0;
      this.lastUpdate = Date.now();
      this.lastProgressAt = this.lastUpdate;
      this.phase = "transferring";
      this.requestDispatched = false;
      this.uploadRequestPhase = "resume";
      const request = createUploadRequest({
        responseLimit: ERROR_RESPONSE_BODY_LIMIT,
        onProgress: () => {},
        onBodySent: () => this.beginCommitWait(),
        onResponse: /** @param {XMLHttpRequest} response */ response =>
          this.handleUploadResponse(response),
        onNetworkError: () => this.handleNetworkError(),
        onAbort: () => this.handleAbort(),
        onOversizedResponse: () => this.handleOversizedResponse(),
      });
      this.startTimeouts(() => request.abort());
      try {
        this.requestDispatched = true;
        dispatchUploadRequest(request, {
          method: "PATCH",
          url: this.url,
          headers: {
            [CSRF_HEADER]: data.csrf_token,
            [UPLOAD_ID_HEADER]: this.uploadId,
            [UPLOAD_LENGTH_HEADER]: String(this.file.size),
            [UPLOAD_OFFSET_HEADER]: String(this.file.size),
            [UPLOAD_OVERWRITE_HEADER]: "true",
            [TARGET_REVISION_HEADER]: this.targetRevision,
          },
          body: null,
        });
        this.beginCommitWait();
      } catch (error) {
        this.requestDispatched = false;
        this.clearTimeouts();
        this.fail(errorMessage(error), "query_upload");
      }
    }

    /**
     * @param {string} [skipReason]
     * @param {boolean} [restartAfterDiscard]
     */
    async discardStaged(
      skipReason = "destination exists",
      restartAfterDiscard = false,
    ) {
      this.phase = "checking";
      renderCleanup(this.view, this.name);
      try {
        const response = await requestNoContent(
          "/__dufs__/api/upload/discard",
          {
            method: "POST",
            headers: {
              "Content-Type": "application/json",
              [CSRF_HEADER]: data.csrf_token,
            },
            body: JSON.stringify({
              path: this.logicalPath,
              upload_id: this.uploadId,
            }),
          },
          {
            timeoutMs: STATUS_TIMEOUT_MS,
            timeoutMessage: "Staged-upload cleanup timed out",
            outcomeUnknown: true,
            resultId: this.uploadId,
          },
        );
        await assertDiscardUploadResponse(
          response,
          this.uploadId,
          this.file.size,
        );
        this.finishDiscard(skipReason, restartAfterDiscard);
      } catch (error) {
        if (isAuthenticationError(error)) {
          pauseForAuthentication();
          const csrfFailed = isRequestErrorCode(error, "csrf_failed");
          if (!csrfFailed) onUnauthorized();
          this.fail(csrfFailed ? PAGE_EXPIRED_MESSAGE : AUTH_REQUIRED_MESSAGE);
          return;
        }
        await this.reconcileDiscard(skipReason, restartAfterDiscard);
      }
    }

    /** @param {string} skipReason @param {boolean} restartAfterDiscard */
    async reconcileDiscard(skipReason, restartAfterDiscard) {
      try {
        const response = await requestHead(this.url, {
          headers: { [UPLOAD_ID_HEADER]: this.uploadId },
        }, {
          timeoutMs: STATUS_TIMEOUT_MS,
          timeoutMessage: "Staged-upload cleanup check timed out",
        });
        const classification = classifyUploadResponse({
          phase: "checkpoint",
          status: response.status,
          headers: response.headers,
          expectedUploadId: this.uploadId,
          expectedLength: this.file.size,
        });
        if (["rejected", "not-seen"].includes(classification.kind)) {
          this.finishDiscard(skipReason, restartAfterDiscard);
          return;
        }
        if (classification.kind === "committed") {
          this.complete();
          return;
        }
        if (classification.kind === "authentication") {
          pauseForAuthentication();
          onUnauthorized();
          this.fail(AUTH_REQUIRED_MESSAGE);
          return;
        }
        if (classification.kind === "csrf") {
          pauseForAuthentication();
          this.fail(PAGE_EXPIRED_MESSAGE);
          return;
        }
      } catch (error) {
        if (isAuthenticationError(error)) {
          pauseForAuthentication();
          const csrfFailed = isRequestErrorCode(error, "csrf_failed");
          if (!csrfFailed) onUnauthorized();
          this.fail(csrfFailed ? PAGE_EXPIRED_MESSAGE : AUTH_REQUIRED_MESSAGE);
          return;
        }
      }
      this.fail(
        restartAfterDiscard
          ? "The destination was not overwritten, but staged-upload cleanup could not be confirmed; check its status before retrying"
          : "File was not overwritten, but cleanup of its staged upload could not be confirmed; the server will expire it automatically",
        restartAfterDiscard ? "retry" : "",
      );
    }

    /** @param {string} skipReason @param {boolean} restartAfterDiscard */
    finishDiscard(skipReason, restartAfterDiscard) {
      if (!restartAfterDiscard) {
        this.skipConflict(skipReason);
        return;
      }
      this.targetRevision = null;
      this.restartSession();
      this.sendBody();
    }

    /** @param {string} [reason] */
    skipConflict(reason = "destination exists") {
      if (this.state !== "running") return;
      this.clearTimeouts();
      this.clearRecovery();
      this.state = "cancelled";
      this.phase = "cancelled";
      knownTargets.delete(this.name);
      renderSkipped(this.view, this.name, reason);
      retainTerminalRow(this);
      this.finishRunning();
    }

    cancelRemainingBatch() {
      const batch = batches.get(this.batchId);
      if (!batch) return;
      let cancelled = batch.cancelRequested ? 0 : batch.remainingEntries;
      batch.cancelRequested = true;
      cancellingBatch = true;
      try {
        for (const uploader of [...batch.members]) {
          if (uploader === this || uploader.state !== "queued") continue;
          uploader.cancel();
          cancelled++;
        }
      } finally {
        cancellingBatch = false;
      }
      if (cancelled > 0) {
        queueMessage.textContent =
          `Cancelled ${cancelled} remaining queued upload${cancelled === 1 ? "" : "s"}. Uploads already in progress were not interrupted.`;
        queueMessage.classList.remove("hidden");
      }
      runQueue();
    }

    handleNetworkError() {
      const outcomeUnknown = this.requestDispatched;
      this.clearTimeouts();
      if (outcomeUnknown) {
        this.unknown(UNKNOWN_UPLOAD_RESULT_MESSAGE, "query_upload");
      } else {
        this.fail("Network connection lost", "retry");
      }
    }

    handleAbort() {
      const outcomeUnknown = this.abortOutcomeUnknown || this.requestDispatched;
      const reason = this.abortReason ||
        (outcomeUnknown ? UNKNOWN_UPLOAD_RESULT_MESSAGE : "Upload cancelled");
      this.clearTimeouts();
      if (outcomeUnknown) {
        this.unknown(reason, "query_upload");
      } else {
        this.fail(reason, "retry");
      }
    }

    handleOversizedResponse() {
      if (this.state !== "running") return;
      this.clearTimeouts();
      this.unknown(
        "The server response exceeded the allowed size",
        "query_upload",
      );
    }

    /** @param {UploadRecoveryAction} recovery */
    recover(recovery) {
      if (
        this.recoveryAction !== recovery ||
        Date.now() < this.recoveryAvailableAt
      ) {
        return;
      }
      const resolvingUnknown = unresolvedUnknown.has(this.index);
      if (
        queueState !== "running" &&
        !(recovery === "query_upload" && resolvingUnknown)
      ) {
        return;
      }
      // A terminal row no longer consumes pending capacity. Reserve its slot
      // before mutating the failure/history state so a rejected recovery stays
      // fully recoverable and cannot push the nonterminal count past the cap.
      if (!this.pendingAccounted && !hasPendingCapacity()) {
        showPendingLimitMessage();
        return;
      }
      if (knownTargets.has(this.name)) {
        queueMessage.textContent =
          `Another upload already targets ${this.name}. Wait for it to finish before retrying this upload.`;
        queueMessage.classList.remove("hidden");
        return;
      }
      if (!failed.delete(this.index)) return;
      knownTargets.add(this.name);
      this.clearRecovery();
      if (resolvingUnknown) {
        releaseTerminalRow(this);
        if (!this.pendingAccounted) {
          this.pendingAccounted = true;
          pendingRows++;
        }
        this.state = "running";
        running++;
        this.runningAccounted = true;
        void this.queryCheckpoint(true);
        return;
      }
      this.enqueue(recovery);
    }

    async queryCheckpoint(queryingUnknown = false, retryAfterCheck = false) {
      const controller = new AbortController();
      this.abortController = controller;
      this.abortReason = "";
      this.abortOutcomeUnknown = false;
      this.phase = "checking";
      this.requestDispatched = false;
      renderCheckpoint(this.view, this.name);
      try {
        const response = await requestHead(this.url, {
          signal: controller.signal,
          headers: { [UPLOAD_ID_HEADER]: this.uploadId },
        }, {
          timeoutMs: STATUS_TIMEOUT_MS,
          timeoutMessage: "Resume status check timed out",
        });
        const classification = classifyUploadResponse({
          phase: "checkpoint",
          status: response.status,
          headers: response.headers,
          expectedUploadId: this.uploadId,
          expectedLength: this.file.size,
        });
        if (classification.kind === "authentication") {
          pauseForAuthentication();
          onUnauthorized();
          this.fail(AUTH_REQUIRED_MESSAGE);
          return;
        }
        if (classification.kind === "csrf") {
          pauseForAuthentication();
          this.fail(PAGE_EXPIRED_MESSAGE);
          return;
        }
        if (["not-seen", "rejected"].includes(classification.kind)) {
          resolveUnknown(this);
          if (retryAfterCheck) {
            this.restartSession();
            this.sendBody();
            return;
          }
          this.fail(
            "The upload session cannot be resumed; start a new upload session",
            "retry",
          );
          return;
        }
        if (classification.kind === "committed") {
          resolveUnknown(this);
          this.complete();
          return;
        }
        if (classification.kind === "awaiting-confirmation") {
          const rawRevision = readHeaderValue(
            response.headers,
            TARGET_REVISION_HEADER,
          );
          const revision = parseTargetRevision(response.headers);
          const replaceable = parseTargetReplaceable(response.headers);
          if (
            replaceable === null ||
            (rawRevision === null && !replaceable) ||
            (rawRevision !== null && revision === null) ||
            classification.protocol?.offset !== this.file.size
          ) {
            this.unknown(
              `The server returned an invalid overwrite checkpoint. ${RESULT_UNKNOWN_MESSAGE}`,
              "query_upload",
            );
            return;
          }
          resolveUnknown(this);
          if (rawRevision === null) {
            this.missingTargetRetryUsed = false;
            this.retryMissingTarget(true);
            return;
          }
          if (revision === null) {
            this.unknown(
              `The server returned an invalid overwrite checkpoint. ${RESULT_UNKNOWN_MESSAGE}`,
              "query_upload",
            );
            return;
          }
          if (!replaceable) {
            void this.discardStaged("destination cannot be replaced");
            return;
          }
          void this.resolveDestinationConflict(revision, true);
          return;
        }
        if (classification.kind === "running") {
          const offset = classification.protocol?.offset;
          if (typeof offset !== "number" || !Number.isSafeInteger(offset)) {
            this.unknown(
              `The server returned an inconsistent upload checkpoint. ${RESULT_UNKNOWN_MESSAGE}`,
              "query_upload",
            );
            return;
          }
          this.uploadOffset = offset;
          if (this.uploadOffset === this.file.size) {
            resolveUnknown(this);
            this.sendBody(true);
            return;
          }
          resolveUnknown(this);
          if (retryAfterCheck) {
            this.sendBody(true);
            return;
          }
          this.fail(
            "Upload remains resumable from the confirmed checkpoint",
            "retry",
          );
          return;
        }
        if (classification.kind === "unknown") {
          this.unknown(
            "The server recorded an uncertain publication outcome. Refresh the folder and inspect the target before selecting the file again",
          );
          return;
        }
        this.unknown(
          `Upload status could not be safely interpreted (HTTP ${response.status})`,
          "query_upload",
        );
      } catch (error) {
        const reason = this.abortReason || errorMessage(error);
        const retryAfter = requestErrorRetryAfter(error);
        const recovery = checkpointErrorRecovery(error);
        if (queryingUnknown) {
          this.unknown(reason, recovery, retryAfter);
        } else {
          this.fail(reason, recovery, retryAfter);
        }
      } finally {
        if (this.abortController === controller) this.abortController = null;
      }
    }

    restartSession() {
      this.uploadId = crypto.randomUUID();
      this.uploadOffset = 0;
      this.uploadRequestPhase = "fresh";
      this.missingTargetRetryUsed = false;
    }

    /** @param {ProgressEvent} event */
    onProgress(event) {
      if (this.phase !== "transferring") return;
      const now = Date.now();
      this.lastProgressAt = now;
      const elapsed = now - this.lastUpdate;
      if (elapsed < 300) return;
      const speed = (event.loaded - this.uploaded) / elapsed * 1000;
      const [speedValue, speedUnit] = formatFileSize(speed);
      const percent = this.file.size === 0
        ? 100
        : ((event.loaded + this.uploadOffset) / this.file.size) * 100;
      const progress = formatPercent(percent);
      const duration = speed > 0
        ? formatDuration((event.total - event.loaded) / speed)
        : "--:--:--";
      const announcement = Math.min(100, Math.floor(percent / 10) * 10);
      const announceNow = announcement > this.lastAnnouncedProgress;
      if (announceNow) this.lastAnnouncedProgress = announcement;
      renderProgress(
        this.view,
        this.name,
        `${speedValue} ${speedUnit}/s`,
        `${progress} ${duration}`,
        announceNow,
      );
      this.uploaded = event.loaded;
      this.lastUpdate = now;
    }

    beginCommitWait() {
      if (this.state !== "running" || this.phase !== "transferring") return;
      this.phase = "submitting";
      if (this.idleTimer !== null) window.clearTimeout(this.idleTimer);
      if (this.totalTimer !== null) window.clearTimeout(this.totalTimer);
      this.idleTimer = null;
      this.totalTimer = null;
      renderSubmitting(this.view, this.name);

      const controller = this.abortController;
      this.commitTimer = window.setTimeout(() => {
        if (
          !controller ||
          this.abortController !== controller ||
          this.phase !== "submitting"
        ) {
          return;
        }
        this.abortReason = UNKNOWN_UPLOAD_RESULT_MESSAGE;
        this.abortOutcomeUnknown = true;
        controller.abort();
      }, COMMIT_TIMEOUT_MS);
    }

    complete() {
      if (this.state !== "running") return;
      this.clearTimeouts();
      this.clearRecovery();
      this.state = "completed";
      this.phase = "completed";
      renderComplete(this.view, this.name);
      onMutation(MUTATION_EFFECT.COMMITTED);
      retainTerminalRow(this);
      this.finishRunning();
    }

    /**
     * @param {string} [reason]
     * @param {UploadRecoveryAction} [recovery]
     * @param {number | string | null} [retryAfter]
     */
    fail(reason = "", recovery = "", retryAfter = null) {
      if (this.state !== "running") return;
      this.clearTimeouts();
      this.clearRecovery();
      this.state = "failed";
      this.phase = "failed";
      const message = reason || "Upload failed";
      const recoveryButton = this.createRecoveryButton(recovery, retryAfter);
      renderFailure(this.view, this.name, message, recoveryButton);
      retainTerminalRow(
        this,
        recoveryButton
          ? () => {
            this.terminalHistoryEntry = null;
            this.clearRecovery();
          }
          : null,
      );
      this.finishRunning();
    }

    /**
     * @param {string} [reason]
     * @param {UploadRecoveryAction} [recovery]
     * @param {number | string | null} [retryAfter]
     */
    unknown(
      reason = UNKNOWN_UPLOAD_RESULT_MESSAGE,
      recovery = "",
      retryAfter = null,
    ) {
      if (this.state !== "running") return;
      this.clearTimeouts();
      this.clearRecovery();
      this.state = "unknown";
      this.phase = "unknown";
      onMutation(MUTATION_EFFECT.OUTCOME_UNKNOWN);
      pauseForUnknown(this);
      const recoveryButton = recovery === "query_upload"
        ? this.createRecoveryButton(recovery, retryAfter)
        : null;
      renderUnknown(this.view, this.name, reason, recoveryButton);
      retainTerminalRow(
        this,
        recoveryButton
          ? () => {
            this.terminalHistoryEntry = null;
            this.clearRecovery();
          }
          : null,
      );
      this.finishRunning();
    }

    /**
     * @param {UploadRecoveryAction} recovery
     * @param {number | string | null} retryAfter
     * @returns {HTMLButtonElement | null}
     */
    createRecoveryButton(recovery, retryAfter) {
      if (!recovery) return null;
      const label = UPLOAD_RECOVERY_LABELS[recovery];
      this.recoveryAction = recovery;
      const recoveryButton = /** @type {HTMLButtonElement} */ (createElement("button", {
        className: "retry-btn",
        text: "↻",
        attributes: {
          type: "button",
          title: label,
          "aria-label": `${label} ${this.name}`,
          "data-recovery": recovery,
        },
      }));
      recoveryButton.addEventListener(
        "click",
        () => this.recover(recovery),
      );
      failed.set(this.index, this);

      const delaySeconds = normalizeRetryAfter(retryAfter);
      if (delaySeconds === null || delaySeconds === 0) return recoveryButton;
      recoveryButton.setAttribute("disabled", "");
      recoveryButton.title = `${label} after ${delaySeconds} seconds`;
      const delayMs = delaySeconds * 1000;
      if (delayMs > MAX_TIMER_DELAY_MS) {
        this.recoveryAvailableAt = Number.POSITIVE_INFINITY;
        return recoveryButton;
      }
      this.recoveryAvailableAt = Date.now() + delayMs;
      this.recoveryTimer = window.setTimeout(() => {
        if (this.recoveryAction !== recovery) return;
        this.recoveryAvailableAt = 0;
        recoveryButton.removeAttribute("disabled");
        recoveryButton.title = label;
      }, delayMs);
      return recoveryButton;
    }

    clearRecovery() {
      if (this.recoveryTimer !== null) window.clearTimeout(this.recoveryTimer);
      this.recoveryTimer = null;
      this.recoveryAction = "";
      this.recoveryAvailableAt = 0;
      failed.delete(this.index);
    }

    cancel() {
      if (this.state === "queued" && queue.cancel(this.queueEntry)) {
        this.queueEntry = null;
        this.clearRecovery();
        this.state = "cancelled";
        this.phase = "cancelled";
        knownTargets.delete(this.name);
        renderCancelled(this.view, this.name);
        retainTerminalRow(this);
        runQueue();
        return;
      }
      if (this.state !== "running" || !this.abortController) return;
      if (this.requestDispatched) {
        this.abortReason =
          `Upload cancellation was requested, but the server result is unknown. ${RESULT_UNKNOWN_MESSAGE}`;
        this.abortOutcomeUnknown = true;
      } else {
        this.abortReason = "Upload cancelled";
        this.abortOutcomeUnknown = false;
      }
      this.abortController.abort();
    }

    /** @param {EventListener} abortRequest */
    startTimeouts(abortRequest) {
      this.clearTimeouts();
      const controller = new AbortController();
      controller.signal.addEventListener("abort", abortRequest, { once: true });
      this.abortController = controller;
      this.abortReason = "";
      this.abortOutcomeUnknown = false;
      this.lastProgressAt = Date.now();
      this.totalTimer = window.setTimeout(() => {
        this.abortReason = "Upload exceeded the maximum duration";
        this.abortOutcomeUnknown = true;
        controller.abort();
      }, TOTAL_TIMEOUT_MS);
      this.scheduleIdleTimeout(controller, IDLE_TIMEOUT_MS);
    }

    /** @param {AbortController} controller @param {number} delay */
    scheduleIdleTimeout(controller, delay) {
      if (this.idleTimer !== null) window.clearTimeout(this.idleTimer);
      this.idleTimer = window.setTimeout(() => {
        if (this.abortController !== controller) return;
        const remaining = IDLE_TIMEOUT_MS - (Date.now() - this.lastProgressAt);
        if (remaining > 0) {
          this.scheduleIdleTimeout(controller, remaining);
          return;
        }
        this.abortReason = "Upload made no progress for too long";
        this.abortOutcomeUnknown = true;
        controller.abort();
      }, delay);
    }

    clearTimeouts() {
      if (this.idleTimer !== null) window.clearTimeout(this.idleTimer);
      if (this.totalTimer !== null) window.clearTimeout(this.totalTimer);
      if (this.commitTimer !== null) window.clearTimeout(this.commitTimer);
      this.idleTimer = null;
      this.totalTimer = null;
      this.commitTimer = null;
      this.abortController = null;
      this.abortReason = "";
      this.abortOutcomeUnknown = false;
    }

    finishRunning() {
      if (!this.runningAccounted) return;
      this.runningAccounted = false;
      running = Math.max(0, running - 1);
      runQueue();
    }
  }

  /** @param {readonly string[]} paths */
  async function preflight(paths) {
    const { response, payload } = await requestJson(
      "/__dufs__/api/upload/preflight",
      {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          [CSRF_HEADER]: data.csrf_token,
        },
        body: JSON.stringify({ paths }),
      },
      {
        timeoutMs: STATUS_TIMEOUT_MS,
        timeoutMessage: "Upload conflict check timed out",
        outcomeUnknown: false,
      },
    );
    if (!response.ok) await assertResponse(response);
    return parseUploadPreflight(payload, paths);
  }

  /**
   * @param {ReturnType<typeof prepareUploadSelection>} selection
   * @param {Element | null} returnFocus
   */
  async function enqueueSelection(selection, returnFocus) {
    if (!selection.ok) {
      queueMessage.textContent = selection.error;
      queueMessage.classList.remove("hidden");
      return;
    }
    if (selection.entries.length === 0) return;
    if (queueState !== "running") {
      queueMessage.classList.remove("hidden");
      return;
    }

    /** @type {UploadSelectionEntry[]} */
    const accepted = [];
    const batchTargets = new Set();
    let duplicateName = "";
    for (const entry of selection.entries) {
      if (knownTargets.has(entry.name) || batchTargets.has(entry.name)) {
        duplicateName ||= entry.name;
        continue;
      }
      batchTargets.add(entry.name);
      accepted.push(entry);
    }
    if (accepted.length === 0) {
      queueMessage.textContent = duplicateName
        ? `Skipped duplicate upload target ${duplicateName}. Refresh the folder before replacing the same target again.`
        : "No files were selected.";
      queueMessage.classList.remove("hidden");
      return;
    }
    const absolutePaths = accepted.map(
      entry => logicalChildPath(data.href, entry.name),
    );
    const pathEncoder = new TextEncoder();
    const absolutePathBytes = absolutePaths.reduce(
      (total, path) => total + pathEncoder.encode(path).byteLength,
      0,
    );
    if (absolutePathBytes > UPLOAD_BATCH_PATH_BYTES_LIMIT) {
      queueMessage.textContent =
        `Selected upload destinations exceed the ${UPLOAD_BATCH_PATH_BYTES_LIMIT}-byte batch limit in this folder. Split the selection into smaller batches.`;
      queueMessage.classList.remove("hidden");
      return;
    }
    let targets;
    try {
      targets = await preflight(absolutePaths);
    } catch (error) {
      if (isAuthenticationError(error)) {
        pauseForAuthentication();
        if (!isRequestErrorCode(error, "csrf_failed")) onUnauthorized();
        return;
      }
      queueMessage.textContent =
        `Unable to check upload destinations: ${errorMessage(error)}`;
      queueMessage.classList.remove("hidden");
      return;
    }
    if (queueState !== "running") return;

    const blockedNames = [];
    const conflicts = [];
    /** @type {QueuedUploadEntry[]} */
    let uploadEntries = [];
    for (let index = 0; index < accepted.length; index++) {
      const entry = accepted[index];
      const target = targets[index];
      if (!target.replaceable) {
        blockedNames.push(entry.name);
      } else if (target.exists) {
        conflicts.push({ entry, revision: target.revision });
      } else {
        uploadEntries.push({ ...entry, revision: null });
      }
    }

    let skippedConflicts = false;
    if (conflicts.length > 0) {
      const choice = await dialogs.chooseAction({
        title: "Existing upload destinations",
        message:
          `${formatNameSummary(conflicts.map(value => value.entry.name))} ${
            conflicts.length === 1 ? "already exists" : "already exist"
          }. Overwrite only if unchanged since this check, skip ${
            conflicts.length === 1 ? "this file" : "these files"
          }, or cancel the batch.`,
        confirmText: "Overwrite",
        alternateText: "Skip conflicts",
        cancelText: "Cancel upload",
        danger: true,
        returnFocus,
      });
      if (choice === "cancel" || queueState !== "running") return;
      if (choice === "confirm") {
        uploadEntries.push(...conflicts.map(({ entry, revision }) => ({
          ...entry,
          revision,
        })));
      } else {
        skippedConflicts = true;
      }
    }

    const selectionOrder = new Map(
      accepted.map((entry, index) => [entry.name, index]),
    );
    uploadEntries.sort(
      (left, right) =>
        (selectionOrder.get(left.name) ?? 0) -
        (selectionOrder.get(right.name) ?? 0),
    );

    // The preflight request and overwrite dialog both yield to unrelated UI
    // actions. Re-check immediately before reserving the batch so a recovery
    // that claimed the same logical target cannot be admitted concurrently.
    uploadEntries = uploadEntries.filter(entry => {
      if (!knownTargets.has(entry.name)) return true;
      duplicateName ||= entry.name;
      return false;
    });

    if (uploadEntries.length === 0) {
      queueMessage.textContent = duplicateName
        ? `Skipped duplicate upload target ${duplicateName}. Refresh the folder before replacing the same target again.`
        : blockedNames.length > 0
          ? `Skipped ${formatNameSummary(blockedNames)} because the destination cannot be replaced.`
          : "All conflicting upload destinations were skipped.";
      queueMessage.classList.remove("hidden");
      return;
    }
    if (!hasPendingCapacity(uploadEntries.length)) {
      showPendingLimitMessage();
      return;
    }

    // Reserve the entire batch before yielding between DOM chunks. Recovery
    // clicks and later selections therefore cannot race the global row cap.
    pendingRows += uploadEntries.length;

    const batchId = nextBatchId++;
    const batch = {
      members: new Set(),
      enqueueComplete: false,
      cancelRequested: false,
      remainingEntries: uploadEntries.length,
    };
    batches.set(batchId, batch);

    const notices = [];
    if (duplicateName) {
      notices.push(`Skipped duplicate upload target ${duplicateName}.`);
    }
    if (blockedNames.length > 0) {
      notices.push(
        `Skipped ${formatNameSummary(blockedNames)} because the destination cannot be replaced.`,
      );
    }
    if (skippedConflicts) {
      notices.push("Skipped the conflicting upload destinations.");
    }
    if (notices.length > 0) {
      queueMessage.textContent = notices.join(" ");
      queueMessage.classList.remove("hidden");
    } else {
      queueMessage.classList.add("hidden");
    }

    // Claim every logical target before the first chunking yield. Otherwise a
    // recovery click could claim an unmaterialized tail entry. Reservations
    // for entries cancelled before construction are released in `finally`.
    const unmaterializedTargets = new Set(
      uploadEntries.map(entry => entry.name),
    );
    for (const name of unmaterializedTargets) knownTargets.add(name);

    let processed = 0;
    try {
      for (const entry of uploadEntries) {
        if (batch.cancelRequested) break;
        const uploader = new Uploader(
          entry.file,
          entry.name,
          entry.revision,
          batchId,
        );
        uploader.pendingAccounted = true;
        batch.members.add(uploader);
        uploader.enqueue();
        unmaterializedTargets.delete(entry.name);
        batch.remainingEntries--;
        processed++;
        if (processed % ENQUEUE_BATCH_SIZE === 0) await yieldToBrowser();
      }
    } finally {
      for (const name of unmaterializedTargets) knownTargets.delete(name);
      pendingRows = Math.max(0, pendingRows - batch.remainingEntries);
      batch.remainingEntries = 0;
      batch.enqueueComplete = true;
      if (batch.members.size === 0) batches.delete(batchId);
    }
  }

  return Object.freeze({
    /**
     * @param {FileList | File[] | null | undefined} files
     * @param {{ returnFocus?: Element | null }} [addOptions]
     */
    addFiles(files, addOptions = {}) {
      // Capture the live FileList before the input is reset. Validation is
      // bounded and creates no uploader state or DOM.
      const selection = prepareUploadSelection(files);
      const enqueue = () => enqueueSelection(
        selection,
        addOptions.returnFocus || document.activeElement,
      );
      enqueueTail = enqueueTail.then(enqueue, enqueue);
      return enqueueTail;
    },
    isBusy() {
      return running > 0 || queue.size > 0;
    },
  });
}

/**
 * Accept a target transition only for a canonical, outcome-known response
 * bound to the current upload and complete staged length. An existing target
 * requires a revision; a missing target requires that header to be absent.
 *
 * @param {{
 *   kind: string,
 *   outcomeUnknown: boolean,
 *   protocol: { state: string, length: number | null, offset: number | null } | null,
 * }} classification
 * @param {{ code: string, status: number }} detail
 * @param {Headers | ((name: string) => string | null)} headers
 * @param {number} expectedLength
 * @returns {
 *   | { kind: "exists", revision: string, replaceable: boolean }
 *   | { kind: "missing" }
 *   | { kind: "reset-stage" }
 *   | null
 * }
 */
function trustedUploadTargetChange(
  classification,
  detail,
  headers,
  expectedLength,
) {
  if (
    classification.outcomeUnknown ||
    detail.status !== 409 ||
    !["not-started", "awaiting-confirmation"].includes(classification.kind) ||
    classification.protocol?.state !== classification.kind ||
    classification.protocol.length !== expectedLength ||
    (
      classification.kind === "awaiting-confirmation" &&
      classification.protocol.offset !== expectedLength
    )
  ) {
    return null;
  }
  if (
    detail.code === "upload_metadata_preservation_refused" &&
    classification.kind === "awaiting-confirmation"
  ) {
    return { kind: "reset-stage" };
  }
  const replaceable = parseTargetReplaceable(headers);
  if (detail.code === "destination_exists") {
    const revision = parseTargetRevision(headers);
    return revision === null || replaceable === null
      ? null
      : { kind: "exists", revision, replaceable };
  }
  if (
    detail.code === "upload_target_changed" &&
    replaceable === true &&
    readHeaderValue(headers, TARGET_REVISION_HEADER) === null
  ) {
    return { kind: "missing" };
  }
  return null;
}

/**
 * @param {Headers | ((name: string) => string | null)} headers
 * @param {string} name
 */
function readHeaderValue(headers, name) {
  return typeof headers === "function" ? headers(name) : headers.get(name);
}

/** @param {readonly string[]} names */
function formatNameSummary(names) {
  const visible = names.slice(0, 5).map(name => `"${name}"`);
  const remaining = names.length - visible.length;
  return remaining > 0
    ? `${visible.join(", ")} and ${remaining} more`
    : visible.join(", ");
}

/**
 * @param {string} state
 * @returns {string}
 */
function uploadFailureMessage(state) {
  switch (state) {
    case "running":
      return "Upload remains resumable";
    case "rejected":
      return "The upload session was rejected";
    case "not-seen":
      return "The upload was not recorded";
    case "not-started":
      return "The upload was not started";
    default:
      return "";
  }
}

/**
 * @param {unknown} recovery
 * @param {string} state
 * @param {{ state: string, offset: number | null } | null} protocol
 * @returns {UploadRecoveryAction}
 */
function uploadRecoveryAction(recovery, state, protocol) {
  if (recovery === "query_upload") return recovery;
  if (!protocol || protocol.state !== state) return "";
  if (recovery === "retry" && state === "not-started") return "retry";
  if (
    recovery === "retry_with_new_id" &&
    ["rejected", "not-seen"].includes(state)
  ) {
    return "retry";
  }
  if (
    recovery === "resume_upload" &&
    state === "running" &&
    Number.isSafeInteger(protocol.offset)
  ) {
    return "retry";
  }
  return "";
}

/**
 * @param {XMLHttpRequest} request
 * @param {unknown} problemRetryAfter
 * @returns {number | null}
 */
function responseRetryAfter(request, problemRetryAfter) {
  const rawHeader = request.getResponseHeader("Retry-After");
  return rawHeader === null
    ? normalizeRetryAfter(problemRetryAfter)
    : normalizeRetryAfter(rawHeader);
}

/** @param {unknown} error @returns {number | null} */
function requestErrorRetryAfter(error) {
  const retryAfter = error && typeof error === "object"
    ? /** @type {Record<string, unknown>} */ (error).retryAfter
    : null;
  return normalizeRetryAfter(retryAfter);
}

/** @param {unknown} error @returns {UploadRecoveryAction} */
function checkpointErrorRecovery(error) {
  if (!error || typeof error !== "object") return "query_upload";
  const detail = /** @type {Record<string, unknown>} */ (error);
  if (detail.recovery) {
    return detail.recovery === "query_upload" ? "query_upload" : "";
  }
  return detail.kind === "http" || detail.status ? "" : "query_upload";
}

/** @param {unknown} value @returns {number | null} */
function normalizeRetryAfter(value) {
  if (typeof value === "string") {
    if (!/^(0|[1-9][0-9]*)$/.test(value)) return null;
    value = Number(value);
  }
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0
    ? value
    : null;
}

/** @param {unknown} value @returns {number} */
function normalizeConcurrency(value) {
  return typeof value === "number" &&
    Number.isSafeInteger(value) &&
    value > 0 &&
    value <= MAX_CLIENT_UPLOAD_CONCURRENCY
    ? value
    : DEFAULT_MAX_CONCURRENT_UPLOADS;
}

/** @param {unknown} value */
function normalizeTerminalRowLimit(value) {
  if (value === undefined) return UPLOAD_TERMINAL_ROW_LIMIT;
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value <= 0) {
    throw new TypeError("Upload terminal row limit must be a positive integer");
  }
  return value;
}

/**
 * @param {HTMLTableRowElement} row
 * @returns {HTMLAnchorElement | null}
 */
function adjacentUploadNameLink(row) {
  for (const sibling of [row.nextElementSibling, row.previousElementSibling]) {
    if (!(sibling instanceof HTMLTableRowElement)) continue;
    const link = sibling.querySelector(".cell-name a");
    if (link instanceof HTMLAnchorElement) return link;
  }
  return null;
}

function yieldToBrowser() {
  return new Promise(resolve => {
    window.requestAnimationFrame(() => resolve(undefined));
  });
}

/** @param {number} seconds */
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

/** @param {number} percent */
function formatPercent(percent) {
  return `${percent > 10 ? percent.toFixed(1) : percent.toFixed(2)}%`;
}
