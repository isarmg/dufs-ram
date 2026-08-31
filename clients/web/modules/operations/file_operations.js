import {
  CSRF_HEADER,
  OPERATION_ID_HEADER,
  RequestError,
  SOURCE_REVISION_HEADER,
  assertDiscardUploadResponse,
  assertFreshUploadResponse,
  assertResponse,
  isAuthenticationError,
  postJson,
  queryUnknownUpload,
  requestJson,
  requestNoContent,
  runMutationWithReconciliation,
} from "../http/client.js";
import { errorMessage } from "../shared/dom.js";
import { MUTATION_EFFECT } from "../shared/mutation_effect.js";
import {
  browserUrlFromLogicalPath,
  childUrl,
  isValidLogicalPath,
  logicalChildPath,
} from "../shared/path.js";
import {
  UPLOAD_ID_HEADER,
  UPLOAD_LENGTH_HEADER,
  UPLOAD_OVERWRITE_HEADER,
} from "../upload/protocol.js";
import { parseUploadPreflight } from "../upload/preflight.js";

const DEFAULT_FOLDER_NAME = "newfolder";
const DEFAULT_FILE_NAME = "newfile";
const DEFAULT_NAME_ATTEMPT_LIMIT = 1_000;
const REVISION_REFRESH_CODES = Object.freeze([
  "delete_target_changed",
  "source_changed",
  "destination_changed",
]);
/** @type {Readonly<Record<string, number>>} */
const ERROR_CODE_STATUS = Object.freeze({
  path_exists: 409,
  destination_exists: 409,
  delete_target_changed: 412,
  source_changed: 412,
  destination_changed: 412,
});

/** @typedef {"succeeded" | "retry" | "refresh" | "unknown" | "authentication"} InlineRenameResult */

/**
 * @typedef {{
 *   href: string,
 *   dir_exists: boolean,
 *   session: {
 *     authenticated: true,
 *     user_id: string,
 *     username: string,
 *     role: "admin",
 *     csrf_token: string,
 *   },
 * }} IndexData
 */

/** @typedef {import("./dialogs.js").ActionDialogs} ActionDialogs */

/**
 * @typedef {{
 *   data: IndexData,
 *   listing: {
 *     getItem: (index: number) => ({ name: string, path_type: string, revision: string } | null),
 *     addCreatedItem: (file: { path_type: "Dir" | "File", name: string, mtime: number, size: number, revision: string }) => number,
 *     remove: (index: number) => void,
 *     removeByName: (name: string) => void,
 *     notifyMutation: (effect: (typeof MUTATION_EFFECT)[keyof typeof MUTATION_EFFECT]) => boolean,
 *     refreshFromFirstPage: () => Promise<void>,
 *     settleInlineRename: () => Promise<boolean>,
 *     startInlineRename: (index: number, returnFocus?: Element | null, options?: { created?: boolean }) => Promise<boolean>,
 *   },
 *   dialogs: ActionDialogs,
 *   operationStatus: HTMLElement,
 *   onUnauthorized: () => void,
 * }} FileOperationsOptions
 */

/** @param {FileOperationsOptions} options */
export function createFileOperations(options) {
  const {
    data,
    listing,
    dialogs,
    operationStatus,
    onUnauthorized,
  } = options;
  /** @type {Map<string, { control: Element | null, message: string }>} */
  const pending = new Map();

  /** @param {number} index */
  async function deletePath(index) {
    const file = listing.getItem(index);
    if (!file || !isValidLogicalPath(file.name)) return;
    const source = logicalPath(file.name);
    const pendingKey = `path:${source}`;
    if (pending.has(pendingKey)) return;
    const returnFocus = document.getElementById(`deleteBtn${index}`);
    if (!await dialogs.confirmAction({
      title: "Delete item",
      message: `Delete "${file.name}"? This action cannot be undone.`,
      confirmText: "Delete",
      danger: true,
      returnFocus,
    })) return;
    if (!begin(pendingKey, returnFocus, `Deleting ${file.name}…`)) return;
    try {
      const operationId = crypto.randomUUID();
      const result = await runMutationWithReconciliation(
        () => requestNoContent(childUrl(file.name), {
          method: "DELETE",
          headers: {
            [CSRF_HEADER]: data.session.csrf_token,
            [OPERATION_ID_HEADER]: operationId,
            "If-Match": `"${file.revision}"`,
          },
        }, {
          outcomeUnknown: true,
          operationId,
        }),
        onUnauthorized,
      );
      const effect = trackedMutationEffect(result);
      listing.notifyMutation(effect);
      if (result.kind === "authentication") return;
      if (result.kind === "succeeded") {
        listing.removeByName(file.name);
        await listing.refreshFromFirstPage();
        return;
      }
      await dialogs.showMessage({
        title: "Delete failed",
        message: `Unable to delete "${file.name}": ${
          result.status?.message || errorMessage(result.error)
        }`,
        returnFocus,
      });
      if (effect === MUTATION_EFFECT.REFRESH_REQUIRED) {
        await listing.refreshFromFirstPage();
      }
    } finally {
      end(pendingKey);
    }
  }

  /**
   * @param {number} index
   * @param {string} name
   * @param {Element | null} returnFocus
   * @returns {Promise<InlineRenameResult>}
   */
  async function renamePath(index, name, returnFocus) {
    const file = listing.getItem(index);
    if (!file || !isValidLogicalPath(file.name)) return "unknown";
    const source = logicalPath(file.name);
    const currentName = logicalBasename(source);
    const pendingKey = `path:${source}`;
    if (pending.has(pendingKey)) return "retry";
    if (name === currentName) return "succeeded";
    if (!isValidLogicalPath(name) || name.includes("/")) {
      return "retry";
    }
    const destination = logicalChild(logicalParent(source), name);
    return await relocatePath({
      endpoint: "rename",
      request: {
        source,
        name,
        source_revision: file.revision,
        overwrite: false,
      },
      source,
      destination,
      fileName: file.name,
      returnFocus,
      verb: "rename",
      onSuccess: () => {},
    });
  }

  /** @param {number} index */
  async function movePath(index) {
    const file = listing.getItem(index);
    if (!file || !isValidLogicalPath(file.name)) return;
    const source = logicalPath(file.name);
    const pendingKey = `path:${source}`;
    if (pending.has(pendingKey)) return;
    const sourceName = logicalBasename(source);
    const currentDirectory = logicalParent(source);
    const returnFocus = document.getElementById(`moveBtn${index}`);
    let directory = await dialogs.requestText({
      title: "Move item",
      message: "Enter an existing destination folder under the shared root. The item name will not change.",
      label: "Destination folder",
      value: currentDirectory,
      confirmText: "Move",
      returnFocus,
    });
    if (directory === null || directory === "") return;
    if (!directory.startsWith("/")) directory = `/${directory}`;
    while (directory.length > 1 && directory.endsWith("/")) {
      directory = directory.slice(0, -1);
    }
    if (
      directory !== "/" &&
      !isValidLogicalPath(directory.slice(1))
    ) {
      await dialogs.showMessage({
        title: "Move failed",
        message: "The destination folder path is invalid.",
        returnFocus,
      });
      return;
    }
    const destination = logicalChild(directory, sourceName);
    if (source === destination) return;
    await relocatePath({
      endpoint: "move",
      request: {
        source,
        directory,
        source_revision: file.revision,
        overwrite: false,
      },
      source,
      destination,
      fileName: file.name,
      returnFocus,
      verb: "move",
      onSuccess() {
        location.href = browserUrlFromLogicalPath(directory);
      },
    });
  }

  /**
   * @param {{
   *   endpoint: "move" | "rename",
   *   request: (
   *     { source: string, directory: string, source_revision: string, overwrite: boolean, destination_revision?: string } |
   *     { source: string, name: string, source_revision: string, overwrite: boolean, destination_revision?: string }
   *   ),
   *   source: string,
   *   destination: string,
   *   fileName: string,
   *   returnFocus: Element | null,
   *   verb: "move" | "rename",
   *   onSuccess: () => void | Promise<void>,
   * }} options
   * @returns {Promise<InlineRenameResult>}
   */
  async function relocatePath(options) {
    const {
      endpoint,
      request,
      source,
      destination,
      fileName,
      returnFocus,
      verb,
      onSuccess,
    } = options;
    const pendingKey = `path:${source}`;
    const presentParticiple = verb === "rename" ? "Renaming" : "Moving";
    const titleVerb = verb === "rename" ? "Rename" : "Move";
    if (!begin(pendingKey, returnFocus, `${presentParticiple} ${fileName}…`)) {
      return "retry";
    }

    try {
      let result = await runMutationWithReconciliation(
        () => postBrowserApi(endpoint, request),
        onUnauthorized,
      );
      if (result.kind === "authentication") return "authentication";
      const destinationRevision = trackedConflictRevision(
        result,
        "destination_exists",
        "targetRevision",
      );
      if (destinationRevision) {
        if (!await dialogs.confirmAction({
          title: "Overwrite destination?",
          message: `Replace "${destination}" with "${source}"?`,
          confirmText: "Overwrite",
          danger: true,
          returnFocus,
        })) return "retry";
        const overwriteRequest = {
          ...request,
          overwrite: true,
          destination_revision: destinationRevision,
        };
        result = await runMutationWithReconciliation(
          () => postBrowserApi(endpoint, overwriteRequest),
          onUnauthorized,
        );
        if (result.kind === "authentication") return "authentication";
      }
      const effect = trackedMutationEffect(result);
      listing.notifyMutation(effect);
      if (result.kind !== "succeeded") {
        await dialogs.showMessage({
          title: `${titleVerb} failed`,
          message: `Unable to ${verb} "${source}" to "${destination}": ${
            result.status?.message || errorMessage(result.error)
          }`,
          returnFocus,
        });
        if (effect === MUTATION_EFFECT.REFRESH_REQUIRED) {
          if (verb === "rename") return "refresh";
          await listing.refreshFromFirstPage();
          return "retry";
        }
        return effect === MUTATION_EFFECT.OUTCOME_UNKNOWN
          ? "unknown"
          : "retry";
      }
      await onSuccess();
      return "succeeded";
    } catch (error) {
      if (isAuthenticationError(error)) return "authentication";
      listing.notifyMutation(MUTATION_EFFECT.OUTCOME_UNKNOWN);
      await dialogs.showMessage({
        title: `${titleVerb} failed`,
        message: `Unable to ${verb} "${source}" to "${destination}": ${errorMessage(error)}`,
        returnFocus,
      });
      return "unknown";
    } finally {
      end(pendingKey);
    }
  }

  async function logout() {
    const returnFocus = document.querySelector(".logout-btn");
    if (!begin("logout", returnFocus, "Signing out…")) return;
    try {
      const response = await requestNoContent("/api/v2/auth/logout", {
        method: "POST",
        headers: {
          [CSRF_HEADER]: data.session.csrf_token,
        },
      }, {
        outcomeUnknown: true,
      });
      await assertResponse(response, onUnauthorized);
      onUnauthorized();
    } catch (error) {
      if (isAuthenticationError(error)) return;
      await dialogs.showMessage({
        title: "Sign out failed",
        message: `Unable to sign out: ${errorMessage(error)}`,
        returnFocus,
      });
    } finally {
      end("logout");
    }
  }

  /** @param {Element | null} returnFocus */
  async function createDefaultFolder(returnFocus) {
    if (!await listing.settleInlineRename()) return;
    if (!begin("create-item", returnFocus, "Creating a new folder…")) return;
    try {
      for (let attempt = 0; attempt < DEFAULT_NAME_ATTEMPT_LIMIT; attempt++) {
        const name = defaultCandidate(DEFAULT_FOLDER_NAME, attempt);
        const result = await runMutationWithReconciliation(
          () => postBrowserApi("mkdir", { path: logicalPath(name) }),
          onUnauthorized,
        );
        if (result.kind === "authentication") return;
        if (result.kind === "succeeded") {
          listing.notifyMutation(MUTATION_EFFECT.COMMITTED);
          await showCreatedItem(name, "Dir", returnFocus);
          return;
        }
        if (isDefiniteTrackedConflict(result, "path_exists")) continue;

        listing.notifyMutation(trackedMutationEffect(result));
        await dialogs.showMessage({
          title: "Create folder failed",
          message: `Unable to create folder "${name}": ${
            result.status?.message || errorMessage(result.error)
          }`,
          returnFocus,
        });
        return;
      }
      await showDefaultNameLimit("folder", returnFocus);
    } finally {
      end("create-item");
    }
  }

  /** @param {Element | null} returnFocus */
  async function createDefaultFile(returnFocus) {
    if (!await listing.settleInlineRename()) return;
    if (!begin("create-item", returnFocus, "Creating a new empty file…")) return;
    try {
      for (let attempt = 0; attempt < DEFAULT_NAME_ATTEMPT_LIMIT; attempt++) {
        const name = defaultCandidate(DEFAULT_FILE_NAME, attempt);
        const uploadId = crypto.randomUUID();
        const targetUrl = childUrl(name);
        try {
          const response = await requestNoContent(targetUrl, {
            method: "PUT",
            headers: {
              [CSRF_HEADER]: data.session.csrf_token,
              [UPLOAD_ID_HEADER]: uploadId,
              [UPLOAD_LENGTH_HEADER]: "0",
              [UPLOAD_OVERWRITE_HEADER]: "false",
            },
            body: "",
          }, {
            outcomeUnknown: true,
            resultId: uploadId,
          });
          await assertFreshUploadResponse(
            response,
            uploadId,
            0,
            onUnauthorized,
          );
          listing.notifyMutation(MUTATION_EFFECT.COMMITTED);
          await showCreatedItem(name, "File", returnFocus);
          return;
        } catch (error) {
          if (isAuthenticationError(error)) return;
          if (isDefiniteFreshUploadConflict(error)) continue;
          if (isAwaitingUploadConflict(error, uploadId)) {
            if (await discardStagedUpload(name, uploadId, returnFocus)) continue;
            return;
          }

          const status = await queryUnknownUpload(
            error,
            targetUrl,
            0,
            onUnauthorized,
          );
          if (status?.authenticationFailed) return;
          if (status?.state === "committed") {
            listing.notifyMutation(MUTATION_EFFECT.COMMITTED);
            await showCreatedItem(name, "File", returnFocus);
            return;
          }
          if (status?.state === "awaiting-confirmation") {
            if (await discardStagedUpload(name, uploadId, returnFocus)) continue;
            return;
          }

          listing.notifyMutation(uploadMutationEffect(error, status));
          await dialogs.showMessage({
            title: "Create file failed",
            message: `Unable to create file "${name}": ${
              status?.message || errorMessage(error)
            }`,
            returnFocus,
          });
          return;
        }
      }
      await showDefaultNameLimit("file", returnFocus);
    } finally {
      end("create-item");
    }
  }

  /**
   * @param {string} name
   * @param {"Dir" | "File"} pathType
   * @param {Element | null} returnFocus
   */
  async function showCreatedItem(name, pathType, returnFocus) {
    const path = logicalPath(name);
    let revision;
    try {
      const { response, payload } = await requestJson(
        "/__dufs__/api/upload/preflight",
        {
          method: "POST",
          headers: {
            "Content-Type": "application/json",
            [CSRF_HEADER]: data.session.csrf_token,
          },
          body: JSON.stringify({ paths: [path] }),
        },
        { outcomeUnknown: false },
      );
      await assertResponse(response, onUnauthorized);
      const [target] = parseUploadPreflight(payload, [path]);
      revision = target.exists ? target.revision : null;
    } catch (error) {
      if (isAuthenticationError(error)) return;
      await listing.refreshFromFirstPage();
      return;
    }
    if (revision === null) {
      await listing.refreshFromFirstPage();
      return;
    }
    const index = listing.addCreatedItem({
      path_type: pathType,
      name,
      mtime: Date.now(),
      size: 0,
      revision,
    });
    await listing.startInlineRename(index, returnFocus, { created: true });
  }

  /**
   * A staged conflict owns server-side temporary state. Only the bound 204 +
   * rejected upload envelope proves that state was discarded; every other
   * result stops candidate generation so a later upload ID cannot leak or
   * bypass the staged session.
   *
   * @param {string} name
   * @param {string} uploadId
   * @param {Element | null} returnFocus
   */
  async function discardStagedUpload(name, uploadId, returnFocus) {
    try {
      const response = await requestNoContent(
        "/__dufs__/api/upload/discard",
        {
          method: "POST",
          headers: {
            "Content-Type": "application/json",
            [CSRF_HEADER]: data.session.csrf_token,
          },
          body: JSON.stringify({
            path: logicalPath(name),
            upload_id: uploadId,
          }),
        },
        {
          outcomeUnknown: true,
          resultId: uploadId,
        },
      );
      await assertDiscardUploadResponse(
        response,
        uploadId,
        0,
        onUnauthorized,
      );
      return true;
    } catch (error) {
      if (isAuthenticationError(error)) return false;
      listing.notifyMutation(MUTATION_EFFECT.OUTCOME_UNKNOWN);
      await dialogs.showMessage({
        title: "Create file failed",
        message:
          `Unable to confirm cleanup for staged file "${name}": ${errorMessage(error)}`,
        returnFocus,
      });
      return false;
    }
  }

  /** @param {"file" | "folder"} kind @param {Element | null} returnFocus */
  async function showDefaultNameLimit(kind, returnFocus) {
    await dialogs.showMessage({
      title: `Create ${kind} failed`,
      message:
        `No available default ${kind} name was found after ${DEFAULT_NAME_ATTEMPT_LIMIT} attempts.`,
      returnFocus,
    });
  }

  /**
   * @param {string} key
   * @param {Element | null} control
   * @param {string} message
   */
  function begin(key, control, message) {
    if (pending.has(key)) return false;
    pending.set(key, { control, message });
    setControlBusy(control, true);
    renderOperationStatus();
    return true;
  }

  /** @param {string} key */
  function end(key) {
    const operation = pending.get(key);
    if (!operation) return;
    pending.delete(key);
    setControlBusy(operation.control, false);
    renderOperationStatus();
  }

  function renderOperationStatus() {
    const latest = Array.from(pending.values()).at(-1);
    operationStatus.textContent = latest?.message || "";
    operationStatus.classList.toggle("hidden", !latest);
  }

  /** @param {string} name */
  function logicalPath(name) {
    return logicalChildPath(data.href, name);
  }

  /** @param {string} path */
  function logicalBasename(path) {
    return path.slice(path.lastIndexOf("/") + 1);
  }

  /** @param {string} path */
  function logicalParent(path) {
    const separator = path.lastIndexOf("/");
    return separator <= 0 ? "/" : path.slice(0, separator);
  }

  /** @param {string} parent @param {string} name */
  function logicalChild(parent, name) {
    return parent === "/" ? `/${name}` : `${parent}/${name}`;
  }

  /** @param {string} action @param {unknown} body */
  function postBrowserApi(action, body) {
    return postJson(
      `/__dufs__/api/${action}`,
      data.session.csrf_token,
      body,
    );
  }

  return Object.freeze({
    createDefaultFile,
    createDefaultFolder,
    deletePath,
    logout,
    movePath,
    renamePath,
  });
}

/** @param {string} base @param {number} attempt */
function defaultCandidate(base, attempt) {
  return attempt === 0 ? base : `${base} (${attempt + 1})`;
}

/**
 * @param {Awaited<ReturnType<typeof runMutationWithReconciliation>>} result
 * @param {string} code
 */
function isDefiniteTrackedConflict(result, code) {
  const expectedStatus = ERROR_CODE_STATUS[code];
  if (!expectedStatus) return false;
  if (result.kind !== "failed") return false;
  if (result.status?.kind === "failed") {
    return result.status.code === code &&
      result.status.status === expectedStatus;
  }
  return result.status === null &&
    result.error instanceof RequestError &&
    result.error.code === code &&
    result.error.status === expectedStatus &&
    (
      !result.error.problemStatus ||
      result.error.problemStatus === expectedStatus
    ) &&
    !result.error.outcomeUnknown &&
    (
      !result.error.operationState ||
      ["failed", "rejected"].includes(result.error.operationState)
    );
}

/**
 * Return conditional authority only from the direct, definite failure that
 * carried it. A reconciled job result cannot safely recreate response headers.
 *
 * @param {Awaited<ReturnType<typeof runMutationWithReconciliation>>} result
 * @param {string} code
 * @param {"sourceRevision" | "targetRevision"} field
 */
function trackedConflictRevision(result, code, field) {
  return isDefiniteTrackedConflict(result, code) &&
      result.kind === "failed" &&
      result.status === null &&
      result.error instanceof RequestError
    ? result.error[field]
    : "";
}

/** @param {unknown} error */
function isDefiniteFreshUploadConflict(error) {
  return error instanceof RequestError &&
    error.code === "destination_exists" &&
    error.status === ERROR_CODE_STATUS.destination_exists &&
    (
      !error.problemStatus ||
      error.problemStatus === ERROR_CODE_STATUS.destination_exists
    ) &&
    !error.outcomeUnknown &&
    ["not-started", "rejected"].includes(
      error.uploadState || error.operationState,
    );
}

/** @param {unknown} error @param {string} uploadId */
function isAwaitingUploadConflict(error, uploadId) {
  return error instanceof RequestError &&
    !error.outcomeUnknown &&
    error.uploadId === uploadId &&
    error.uploadLength === 0 &&
    error.uploadState === "awaiting-confirmation";
}

/**
 * Convert the reconciled tracked-operation result into the single listing
 * invalidation protocol. A terminal failed job proves that no successful
 * mutation remains to observe, even if the original transport error was
 * conservative. Running, unknown, or unavailable reconciliation cannot make
 * that guarantee.
 *
 * @param {Awaited<ReturnType<typeof runMutationWithReconciliation>>} result
 * @returns {(typeof MUTATION_EFFECT)[keyof typeof MUTATION_EFFECT]}
 */
export function trackedMutationEffect(result) {
  if (result.kind === "succeeded") return MUTATION_EFFECT.COMMITTED;
  if (result.kind !== "failed") return MUTATION_EFFECT.NOT_COMMITTED;
  if (
    REVISION_REFRESH_CODES.some(code =>
      isDefiniteTrackedConflict(result, code)
    )
  ) {
    return MUTATION_EFFECT.REFRESH_REQUIRED;
  }
  if (
    result.status?.kind === "running" ||
    result.status?.kind === "unknown" ||
    result.status?.kind === "unavailable"
  ) {
    return MUTATION_EFFECT.OUTCOME_UNKNOWN;
  }
  return result.status === null &&
      result.error instanceof RequestError &&
      result.error.outcomeUnknown
    ? MUTATION_EFFECT.OUTCOME_UNKNOWN
    : MUTATION_EFFECT.NOT_COMMITTED;
}

/**
 * Classify an empty-file PUT after its one status reconciliation. Only a
 * matching terminal rejection suppresses invalidation: `not-seen` can race a
 * detached original request that has not persisted its session yet, so it
 * cannot prove that the target will remain unchanged.
 *
 * @param {unknown} error
 * @param {Awaited<ReturnType<typeof queryUnknownUpload>>} status
 * @returns {(typeof MUTATION_EFFECT)[keyof typeof MUTATION_EFFECT]}
 */
export function uploadMutationEffect(error, status) {
  if (status?.state === "committed") return MUTATION_EFFECT.COMMITTED;
  if (status?.state === "rejected") return MUTATION_EFFECT.NOT_COMMITTED;
  if (status?.state === "running" || status?.state === "unknown") {
    return MUTATION_EFFECT.OUTCOME_UNKNOWN;
  }
  return error instanceof RequestError && error.outcomeUnknown
    ? MUTATION_EFFECT.OUTCOME_UNKNOWN
    : MUTATION_EFFECT.NOT_COMMITTED;
}

/** @param {Element | null | undefined} control @param {boolean} busy */
function setControlBusy(control, busy) {
  if (!control || typeof control.setAttribute !== "function") return;
  if (busy) {
    control.setAttribute("aria-busy", "true");
    control.setAttribute("aria-disabled", "true");
  } else {
    control.removeAttribute("aria-busy");
    control.removeAttribute("aria-disabled");
  }
}
