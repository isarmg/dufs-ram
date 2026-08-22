import { createElement, createIcon } from "../shared/dom.js";

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

/**
 * @param {number} index
 * @param {string} name
 * @param {string} url
 * @param {() => void} onCancel
 * @returns {UploadView}
 */
export function createUploadView(index, name, url, onCancel) {
  const row = /** @type {HTMLTableRowElement} */ (createElement("tr", {
    className: "uploader",
    attributes: { id: `upload${index}` },
  }));
  const iconCell = createElement("td", { className: "path cell-icon" });
  iconCell.append(createIcon("file"));
  const nameCell = createElement("td", { className: "path cell-name" });
  nameCell.append(createElement("a", {
    text: name,
    attributes: { href: url },
  }));
  const statusCell = /** @type {HTMLTableCellElement} */ (createElement("td", {
    className: "cell-status upload-status",
    attributes: {
      id: `uploadStatus${index}`,
      "aria-label": `${name}: waiting to upload`,
    },
  }));
  const speedNode = /** @type {HTMLSpanElement} */ (createElement("span", {
    className: "upload-speed",
    attributes: { "aria-hidden": "true" },
  }));
  const progressNode = /** @type {HTMLSpanElement} */ (createElement("span", {
    className: "upload-progress",
    attributes: { "aria-hidden": "true" },
  }));
  const liveNode = /** @type {HTMLSpanElement} */ (createElement("span", {
    className: "visually-hidden",
    text: `${name}: waiting to upload`,
    attributes: { role: "status", "aria-live": "polite" },
  }));
  const cancelButton = /** @type {HTMLButtonElement} */ (createElement("button", {
    className: "upload-cancel",
    text: "Cancel",
    attributes: {
      type: "button",
      "aria-label": `Cancel upload ${name}`,
    },
  }));
  cancelButton.addEventListener("click", onCancel);
  row.append(iconCell, nameCell, statusCell);
  const view = Object.freeze({
    row,
    statusCell,
    speedNode,
    progressNode,
    liveNode,
    cancelButton,
  });
  renderWaiting(view, name);
  return view;
}

/** @param {UploadView} view @param {string} name @param {boolean} [retry] */
export function renderWaiting(view, name, retry = false) {
  setCancelMode(
    view,
    "Cancel",
    `Cancel ${retry ? "queued retry" : "queued upload"} ${name}`,
  );
  const message = retry ? "Waiting to retry" : "Waiting";
  view.statusCell.replaceChildren(
    createElement("span", { text: message }),
    view.cancelButton,
    view.liveNode,
  );
  announce(view, `${name}: ${message.toLowerCase()}`);
}

/**
 * @param {UploadView} view
 * @param {string} name
 * @param {string} speedText
 * @param {string} progressText
 * @param {boolean} announceNow
 */
export function renderProgress(view, name, speedText, progressText, announceNow) {
  setCancelMode(view, "Cancel", `Cancel upload ${name}`);
  if (
    !view.statusCell.contains(view.speedNode) ||
    !view.statusCell.contains(view.progressNode) ||
    !view.statusCell.contains(view.cancelButton) ||
    !view.statusCell.contains(view.liveNode)
  ) {
    // A retry after an overwrite decision re-enters progress rendering from a
    // view that intentionally has no cancel button. Rebuild the owned status
    // nodes atomically instead of using a possibly detached node as the
    // insertBefore reference.
    view.statusCell.replaceChildren(
      view.speedNode,
      view.progressNode,
      view.cancelButton,
      view.liveNode,
    );
  }
  view.speedNode.textContent = speedText;
  view.progressNode.textContent = progressText;
  if (announceNow) {
    announce(view, `${name}: ${progressText} uploaded at ${speedText}`);
  }
}

/** @param {UploadView} view @param {string} name */
export function renderCheckpoint(view, name) {
  setCancelMode(view, "Cancel", `Cancel resume status check for ${name}`);
  view.statusCell.replaceChildren(
    createElement("span", { text: "Checking resume status…" }),
    view.cancelButton,
    view.liveNode,
  );
  announce(view, `${name}: checking resume status`);
}

/** @param {UploadView} view @param {string} name */
export function renderSubmitting(view, name) {
  setCancelMode(view, "Stop waiting", `Stop waiting for upload ${name}`);
  view.statusCell.replaceChildren(
    createElement("span", { text: "Submitting…" }),
    view.cancelButton,
    view.liveNode,
  );
  announce(view, `${name}: upload data sent; waiting for server confirmation`);
}

/** @param {UploadView} view @param {string} name */
export function renderWaitingForOverwrite(view, name) {
  view.statusCell.replaceChildren(
    createElement("span", { text: "Waiting for overwrite decision…" }),
    view.liveNode,
  );
  announce(view, `${name}: waiting for overwrite decision`);
}

/** @param {UploadView} view @param {string} name */
export function renderComplete(view, name) {
  view.statusCell.replaceChildren(
    createElement("span", {
      text: "✓",
      attributes: { "aria-hidden": "true" },
    }),
    view.liveNode,
  );
  announce(view, `${name}: upload complete`);
}

/**
 * @param {UploadView} view
 * @param {string} name
 * @param {string} reason
 * @param {HTMLButtonElement | null} [retryButton]
 */
export function renderFailure(view, name, reason, retryButton = null) {
  view.statusCell.replaceChildren(createElement("span", {
    className: "upload-failure",
    text: `✗ ${reason}`,
    attributes: { title: reason },
  }));
  if (retryButton) view.statusCell.append(retryButton);
  view.statusCell.append(view.liveNode);
  announce(view, `${name}: ${reason}`);
}

/**
 * @param {UploadView} view
 * @param {string} name
 * @param {string} reason
 * @param {HTMLButtonElement | null} [recoveryButton]
 */
export function renderUnknown(view, name, reason, recoveryButton = null) {
  view.statusCell.replaceChildren(
    createElement("span", {
      className: "upload-failure upload-unknown",
      text: `? ${reason}`,
      attributes: { title: reason },
    }),
  );
  if (recoveryButton) view.statusCell.append(recoveryButton);
  view.statusCell.append(view.liveNode);
  announce(view, `${name}: upload result unknown; ${reason}`);
}

/** @param {UploadView} view @param {string} name */
export function renderCancelled(view, name) {
  view.statusCell.replaceChildren(
    createElement("span", { text: "Cancelled" }),
    view.liveNode,
  );
  announce(view, `${name}: upload cancelled`);
}

/** @param {UploadView} view @param {string} name @param {string} reason */
export function renderSkipped(view, name, reason) {
  view.statusCell.replaceChildren(
    createElement("span", { text: `Skipped (${reason})` }),
    view.liveNode,
  );
  announce(view, `${name}: skipped because the ${reason}`);
}

/** @param {UploadView} view @param {string} message */
function announce(view, message) {
  view.statusCell.setAttribute("aria-label", message);
  view.liveNode.textContent = message;
}

/**
 * @param {UploadView} view
 * @param {string} text
 * @param {string} accessibleName
 */
function setCancelMode(view, text, accessibleName) {
  view.cancelButton.textContent = text;
  view.cancelButton.setAttribute("aria-label", accessibleName);
}
