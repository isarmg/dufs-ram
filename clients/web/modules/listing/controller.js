import {
  RequestError,
  assertResponse,
  isAuthenticationError,
  requestJson,
} from "../http/client.js";
import {
  createElement,
  createIcon,
  errorMessage,
  formatFileSize,
} from "../shared/dom.js";
import { MUTATION_EFFECT } from "../shared/mutation_effect.js";
import { childUrl, isValidLogicalPath } from "../shared/path.js";

const LIST_PAGE_LIMIT = 200;
const MAX_CURSOR_LENGTH = 1024;
const MAX_RENDERED_ITEMS = LIST_PAGE_LIMIT;
const PATH_TYPES = new Set(["Dir", "SymlinkDir", "File", "SymlinkFile"]);

/** @param {unknown} error */
function isRefreshableDirectoryChange(error) {
  return error instanceof RequestError &&
    error.status === 409 &&
    error.problemStatus === 409 &&
    error.code === "directory_changed" &&
    error.recovery === "refresh_target";
}

/** @typedef {(typeof MUTATION_EFFECT)[keyof typeof MUTATION_EFFECT]} MutationEffect */

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

/**
 * @typedef {{
 *   path_type: "Dir" | "SymlinkDir" | "File" | "SymlinkFile",
 *   name: string,
 *   mtime: number,
 *   size: number,
 *   revision: string,
 * }} ListingItem
 */

/**
 * @typedef {{
 *   index: number,
 *   sourceName: string,
 *   originalName: string,
 *   input: HTMLInputElement,
 *   error: HTMLElement,
 *   returnFocus: Element | null,
 *   created: boolean,
 *   blurFocus: ({ control: "name" | "download" | "move" | "delete" | "rename" } | { element: Element } | null),
 *   commitPromise: Promise<InlineRenameResult> | null,
 * }} InlineEditor
 */

/**
 * @typedef {{
 *   data: IndexData,
 *   params: { q: string, sort: string, order: string },
 *   table: HTMLTableElement,
 *   tableHead: HTMLTableSectionElement,
 *   tableBody: HTMLTableSectionElement,
 *   emptyFolder: HTMLElement,
 *   emptyNote: string,
 *   loadMore: HTMLButtonElement,
 *   listStatus: HTMLElement,
 *   onAction: (action: string, index: number) => void,
 *   onRename: (index: number, name: string, returnFocus: Element | null) => Promise<InlineRenameResult>,
 *   onUnauthorized: () => void,
 * }} DirectoryListingOptions
 */

/**
 * Keep every operation in a stable visual column. An unavailable action still
 * owns an inert slot, so controls in later columns never shift.
 *
 * @param {"move" | "download" | "delete" | "rename"} name
 * @param {HTMLElement | null} control
 * @returns {HTMLElement}
 */
function createActionSlot(name, control) {
  const slot = createElement("span", {
    className: "action-slot",
    attributes: {
      "data-action-slot": name,
      "aria-hidden": control === null ? "true" : undefined,
    },
  });
  if (control !== null) slot.append(control);
  return slot;
}

/** @param {DirectoryListingOptions} options */
export function createDirectoryListing(options) {
  const {
    data,
    params,
    table,
    tableHead,
    tableBody,
    emptyFolder,
    emptyNote,
    loadMore,
    listStatus,
    onAction,
    onRename,
    onUnauthorized,
  } = options;
  /** @type {string | null} */
  let nextCursor = null;
  let loading = false;
  let loaded = false;
  let invalidated = false;
  let refreshAfterLoad = false;
  let revision = 0;
  let visibleCount = 0;
  let renderedStart = 0;
  let renderedEnd = 0;
  let renderedVisibleCount = 0;
  let invalidationMessage = "";
  let refreshAfterEditor = false;
  /** @type {InlineEditor | null} */
  let activeEditor = null;
  /** @type {(ListingItem | null)[]} */
  const items = [];
  const loadedNames = new Set();
  const seenCursors = new Set();
  const showPrevious = createWindowButton("Show previous items");
  const showNext = createWindowButton("Show next items");
  loadMore.before(showPrevious, showNext);

  function resetListing() {
    activeEditor = null;
    table.classList.remove("has-inline-editor");
    items.length = 0;
    visibleCount = 0;
    renderedStart = 0;
    renderedEnd = 0;
    renderedVisibleCount = 0;
    loadedNames.clear();
    seenCursors.clear();
    nextCursor = null;
    loaded = false;
    invalidated = false;
    invalidationMessage = "";
    refreshAfterEditor = false;
    tableBody.replaceChildren();
    table.classList.add("hidden");
    emptyFolder.classList.add("hidden");
    showPrevious.classList.add("hidden");
    showNext.classList.add("hidden");
  }

  /** @param {Exclude<MutationEffect, "not-committed">} effect */
  function invalidate(effect) {
    revision++;
    invalidated = true;
    nextCursor = null;
    seenCursors.clear();
    loadMore.disabled = loading;
    loadMore.textContent = "Refresh";
    loadMore.classList.remove("hidden");
    invalidationMessage = effect === MUTATION_EFFECT.OUTCOME_UNKNOWN
      ? "Folder contents may have changed; refresh the list before loading more items."
      : effect === MUTATION_EFFECT.REFRESH_REQUIRED
        ? "The folder snapshot is stale; refresh the list before another operation."
        : "Folder contents changed; refresh the list before loading more items.";
    listStatus.textContent = invalidationMessage;
  }

  /**
   * Single cache/DOM invalidation boundary for every browser-side mutation.
   * Known rejections and pre-dispatch failures explicitly pass
   * `NOT_COMMITTED`, preventing conservative timeout handling from turning
   * every failed action into a needless refresh.
   *
   * @param {MutationEffect} effect
   * @returns {boolean} whether the listing was invalidated
   */
  function notifyMutation(effect) {
    switch (effect) {
      case MUTATION_EFFECT.COMMITTED:
      case MUTATION_EFFECT.OUTCOME_UNKNOWN:
      case MUTATION_EFFECT.REFRESH_REQUIRED:
        invalidate(effect);
        return true;
      case MUTATION_EFFECT.NOT_COMMITTED:
        return false;
      default:
        throw new TypeError("Invalid mutation effect");
    }
  }

  async function refreshFromFirstPage() {
    if (activeEditor) {
      refreshAfterEditor = true;
      return;
    }
    const focusAnchor = captureListingFocus();
    revision++;
    if (loading) {
      refreshAfterLoad = true;
      return;
    }
    resetListing();
    await loadNextPage();
    restoreListingFocus(focusAnchor);
  }

  /**
   * Preserve the logical control, rather than a DOM node that the refresh will
   * remove. This keeps keyboard focus on the same item even when its row index
   * changes after a mutation.
   *
   * @returns {{ name: string, control: "name" | "download" | "rename" | "move" | "delete" } | "status" | null}
   */
  function captureListingFocus() {
    const focused = document.activeElement;
    if (!(focused instanceof HTMLElement)) return null;
    if (focused === loadMore || focused === showPrevious || focused === showNext) {
      return "status";
    }
    const row = focused.closest("tr[id^='addPath']");
    if (!(row instanceof HTMLTableRowElement) || !tableBody.contains(row)) {
      return null;
    }
    const rawIndex = row.id.slice("addPath".length);
    const index = Number(rawIndex);
    const item = Number.isSafeInteger(index) ? items[index] : null;
    if (!item) return "status";
    const action = focused.closest("[data-action]")?.getAttribute("data-action");
    const control = action === "rename" || action === "move" || action === "delete"
      ? action
      : focused.closest(".cell-actions") ? "download" : "name";
    return { name: item.name, control };
  }

  /**
   * @param {{ name: string, control: "name" | "download" | "rename" | "move" | "delete" } | "status" | null} anchor
   */
  function restoreListingFocus(anchor) {
    // Do not steal focus if the user moved to another control while the
    // refresh request was in flight. A removed row leaves focus on body.
    if (!anchor || document.activeElement !== document.body) return;
    if (anchor === "status") {
      focusListStatus();
      return;
    }
    const index = items.findIndex(item => item?.name === anchor.name);
    const row = index < 0 ? null : document.getElementById(`addPath${index}`);
    const selector = anchor.control === "name"
      ? ".cell-name a"
      : anchor.control === "download"
        ? ".cell-actions a"
        : `button[data-action="${anchor.control}"]`;
    const target = row?.querySelector(selector);
    if (target instanceof HTMLElement) {
      target.focus({ preventScroll: true });
    } else {
      focusListStatus();
    }
  }

  function focusListStatus() {
    listStatus.tabIndex = -1;
    listStatus.focus({ preventScroll: true });
  }

  function renderHead() {
    const headerItems = [
      { name: "name", colspan: 2, text: "Name" },
      { name: "mtime", text: "Modified" },
      { name: "size", text: "Size" },
    ];
    const row = createElement("tr");
    for (const item of headerItems) {
      let order = "desc";
      let indicator = "↕";
      const active = params.sort === item.name;
      if (active) {
        if (params.order === "desc") {
          order = "asc";
          indicator = "↓";
        } else {
          indicator = "↑";
        }
      }
      const query = new URLSearchParams({
        ...params,
        order,
        sort: item.name,
      }).toString();
      const cell = createElement("th", {
        className: `cell-${item.name}`,
        attributes: {
          scope: "col",
          colspan: item.colspan,
          "aria-sort": active
            ? params.order === "desc" ? "descending" : "ascending"
            : "none",
        },
      });
      const link = createElement("a", {
        text: item.text,
        attributes: {
          href: `?${query}`,
          "aria-label": `${item.text}, ${active ? "change sort direction" : "sort by this column"}`,
        },
      });
      link.append(createElement("span", {
        text: indicator,
        attributes: { "aria-hidden": "true" },
      }));
      cell.append(link);
      row.append(cell);
    }
    row.append(createElement("th", {
      className: "cell-actions",
      text: "Actions",
      attributes: { scope: "col" },
    }));
    tableHead.replaceChildren(row);
  }

  async function loadNextPage() {
    if (invalidated) {
      await refreshFromFirstPage();
      return;
    }
    if (loading || (loaded && nextCursor === null)) return;
    const requestRevision = revision;
    const invokedWithFocus = document.activeElement === loadMore;
    loading = true;
    loadMore.textContent = "Load more";
    loadMore.disabled = true;
    if (!loaded) loadMore.classList.add("hidden");
    listStatus.textContent = loaded ? "Loading more…" : "Loading files…";

    try {
      const url = new URL("/__dufs__/api/list", location.origin);
      url.searchParams.set("path", data.href);
      url.searchParams.set("limit", String(LIST_PAGE_LIMIT));
      if (params.q) url.searchParams.set("q", params.q);
      if (params.sort) url.searchParams.set("sort", params.sort);
      if (params.order) url.searchParams.set("order", params.order);
      if (nextCursor !== null) url.searchParams.set("cursor", nextCursor);

      const requestedCursor = nextCursor;
      let rawPayload;
      for (let attempt = 0; attempt < 2; attempt++) {
        const { response, payload } = await requestJson(url);
        if (requestRevision !== revision) return;
        if (response.status === 409 && loaded) {
          resetListing();
          throw new Error("Folder contents changed. Reload the list and try again.");
        }
        try {
          await assertResponse(response, onUnauthorized);
        } catch (error) {
          if (
            attempt === 0 &&
            requestedCursor === null &&
            isRefreshableDirectoryChange(error)
          ) {
            continue;
          }
          throw error;
        }
        rawPayload = payload;
        break;
      }
      const payload = validateListingPage(rawPayload);
      if (
        payload.nextCursor !== null &&
        (
          payload.nextCursor === requestedCursor ||
          seenCursors.has(payload.nextCursor)
        )
      ) {
        throw new Error("The server repeated a file list cursor");
      }
      for (const file of payload.paths) {
        if (loadedNames.has(file.name)) {
          throw new Error("The server repeated a file list item");
        }
      }

      const firstIndex = items.length;
      const canAppendWithoutWindowing =
        items.length + payload.paths.length <= MAX_RENDERED_ITEMS;
      const fragment = canAppendWithoutWindowing
        ? document.createDocumentFragment()
        : null;
      if (fragment) {
        for (let offset = 0; offset < payload.paths.length; offset++) {
          addPath(payload.paths[offset], firstIndex + offset, fragment);
        }
      }

      items.push(...payload.paths);
      visibleCount += payload.paths.length;
      for (const file of payload.paths) loadedNames.add(file.name);
      nextCursor = payload.nextCursor;
      if (nextCursor !== null) seenCursors.add(nextCursor);
      loaded = true;
      if (fragment) {
        tableBody.append(fragment);
        renderedStart = 0;
        renderedEnd = items.length;
        renderedVisibleCount = visibleCount;
      } else {
        renderWindow(Math.max(0, items.length - MAX_RENDERED_ITEMS));
      }
      updateVisibility(invokedWithFocus);
    } catch (error) {
      if (isAuthenticationError(error)) return;
      listStatus.textContent =
        `Unable to load the file list: ${errorMessage(error)}`;
      loadMore.textContent = "Retry";
      loadMore.classList.remove("hidden");
      if (invokedWithFocus) loadMore.focus();
    } finally {
      loading = false;
      loadMore.disabled = false;
      if (refreshAfterLoad) {
        refreshAfterLoad = false;
        void refreshFromFirstPage();
      }
    }
  }

  function updateVisibility(moveFocusFromLoadMore = false) {
    if (visibleCount > 0) {
      table.classList.remove("hidden");
      emptyFolder.classList.add("hidden");
    } else {
      table.classList.add("hidden");
      emptyFolder.textContent = emptyNote;
      emptyFolder.classList.remove("hidden");
    }

    if (invalidated) {
      loadMore.textContent = "Refresh";
      loadMore.classList.remove("hidden");
      listStatus.textContent = invalidationMessage;
    } else if (nextCursor === null) {
      if (moveFocusFromLoadMore) {
        focusListStatus();
      }
      loadMore.classList.add("hidden");
      listStatus.textContent = visibleCount > 0
        ? appendWindowStatus(`All ${visibleCount} items loaded`)
        : "";
    } else {
      loadMore.classList.toggle("hidden", renderedEnd < items.length);
      listStatus.textContent = appendWindowStatus(
        `${visibleCount} items loaded`,
      );
    }
    showPrevious.classList.toggle("hidden", renderedStart === 0);
    showNext.classList.toggle("hidden", renderedEnd >= items.length);
  }

  /** @param {string} status */
  function appendWindowStatus(status) {
    if (renderedStart === 0 && renderedEnd >= items.length) return status;
    const first = items.length === 0 ? 0 : renderedStart + 1;
    return `${status}; showing items ${first}–${renderedEnd} ` +
      `(${renderedVisibleCount} visible) in this window`;
  }

  /** @param {number} start */
  function renderWindow(start) {
    const maximumStart = Math.max(0, items.length - MAX_RENDERED_ITEMS);
    renderedStart = Math.max(0, Math.min(start, maximumStart));
    renderedEnd = Math.min(items.length, renderedStart + MAX_RENDERED_ITEMS);
    renderedVisibleCount = 0;
    const fragment = document.createDocumentFragment();
    for (let index = renderedStart; index < renderedEnd; index++) {
      const file = items[index];
      if (!file) continue;
      renderedVisibleCount++;
      addPath(file, index, fragment);
    }
    tableBody.replaceChildren(fragment);
  }

  function showPreviousWindow() {
    renderWindow(renderedStart - MAX_RENDERED_ITEMS);
    updateVisibility();
    if (renderedStart === 0) {
      focusListStatus();
    } else {
      showPrevious.focus();
    }
  }

  function showNextWindow() {
    renderWindow(renderedEnd);
    updateVisibility();
    const focusTarget = renderedEnd < items.length
      ? showNext
      : nextCursor === null ? listStatus : loadMore;
    if (focusTarget === listStatus) {
      focusListStatus();
    } else {
      focusTarget.focus();
    }
  }

  /**
   * Add a known-committed item ahead of the current server snapshot. Rendering
   * stays inside the bounded row window, so the transient editor can never
   * create an extra DOM row. A stale row with the same name is replaced rather
   * than duplicated.
   *
   * @param {ListingItem} file
   * @returns {number}
   */
  function addCreatedItem(file) {
    const item = validateCreatedItem(file);
    const hadInlineEditor = activeEditor !== null ||
      tableBody.querySelector(
        ".inline-name-input, .inline-name-error, .is-renaming",
      ) !== null;
    activeEditor = null;
    table.classList.remove("has-inline-editor");
    const staleIndex = items.findIndex(candidate => candidate?.name === item.name);
    const renderedRows = Array.from(tableBody.rows);
    const renderedRowIndices = renderedRows.map(
      row => Number(row.id.match(/^addPath(\d+)$/)?.[1] ?? Number.NaN),
    );
    const canPrepend = !hadInlineEditor && staleIndex < 0 &&
      renderedStart === 0 &&
      new Set(renderedRowIndices).size === renderedRowIndices.length &&
      renderedRowIndices.every((index, position) =>
        Number.isSafeInteger(index) &&
        index >= renderedStart &&
        index < renderedEnd &&
        pathRowUsesIndex(renderedRows[position], index)
      );
    if (staleIndex >= 0) {
      items.splice(staleIndex, 1);
      visibleCount = Math.max(0, visibleCount - 1);
      loadedNames.delete(item.name);
    }
    items.unshift(item);
    visibleCount++;
    loadedNames.add(item.name);
    loaded = true;
    invalidate(MUTATION_EFFECT.COMMITTED);
    if (canPrepend) {
      renderedEnd = Math.min(items.length, MAX_RENDERED_ITEMS);
      const indexedRows = renderedRows.map((row, position) => ({
        row,
        previousIndex: renderedRowIndices[position],
      }));
      indexedRows.sort((left, right) =>
        right.previousIndex - left.previousIndex
      );
      for (const { row, previousIndex } of indexedRows) {
        const nextIndex = previousIndex + 1;
        if (nextIndex >= renderedEnd) {
          row.remove();
        } else {
          reindexPathRow(row, nextIndex);
        }
      }
      tableBody.prepend(createPathRow(item, 0));
      renderedVisibleCount = tableBody.rows.length;
    } else {
      renderWindow(0);
    }
    updateVisibility();
    return 0;
  }

  /**
   * Keep one filename editor for the whole table. Starting another editor
   * first settles the current one, preventing two rows from racing a rename.
   *
   * @param {number} index
   * @param {Element | null} [returnFocus]
   * @param {{ created?: boolean }} [options]
   * @returns {Promise<boolean>}
   */
  async function startInlineRename(index, returnFocus = null, options = {}) {
    if (activeEditor?.index === index) {
      activeEditor.input.focus({ preventScroll: true });
      return true;
    }
    if (!await settleInlineRename()) return false;
    const item = items[index];
    const row = document.getElementById(`addPath${index}`);
    const nameCell = row?.querySelector(".cell-name");
    if (
      !item ||
      !(row instanceof HTMLTableRowElement) ||
      !(nameCell instanceof HTMLTableCellElement) ||
      !tableBody.contains(row)
    ) {
      return false;
    }

    const errorId = `inlineNameError${index}`;
    const originalName = logicalBasename(item.name);
    const input = /** @type {HTMLInputElement} */ (createElement("input", {
      className: "inline-name-input",
      attributes: {
        type: "text",
        autocomplete: "off",
        spellcheck: "false",
        "aria-label": `Rename ${item.name}`,
        "aria-describedby": errorId,
      },
    }));
    input.value = originalName;
    const error = createElement("span", {
      className: "inline-name-error",
      attributes: {
        id: errorId,
        role: "alert",
        hidden: true,
      },
    });
    const editor = /** @type {InlineEditor} */ ({
      index,
      sourceName: item.name,
      originalName,
      input,
      error,
      returnFocus: returnFocus || document.getElementById(`renameBtn${index}`),
      created: Boolean(options.created),
      blurFocus: null,
      commitPromise: null,
    });
    activeEditor = editor;
    table.classList.add("has-inline-editor");
    row.classList.add("is-renaming");
    nameCell.replaceChildren(input, error);

    input.addEventListener("input", () => clearInlineError(editor));
    input.addEventListener("keydown", event => {
      if (event.key === "Escape" && !event.isComposing) {
        event.preventDefault();
        void cancelInlineRename(editor, true);
      } else if (event.key === "Enter" && !event.isComposing) {
        event.preventDefault();
        void commitInlineRename(editor, true);
      }
    });
    input.addEventListener("blur", event => {
      if (editor.blurFocus === null) {
        editor.blurFocus = captureBlurFocus(index, event.relatedTarget);
      }
      void commitInlineRename(editor, false);
    });

    input.focus({ preventScroll: true });
    const isDirectory = item.path_type.endsWith("Dir");
    placeInlineCaret(input, isDirectory);
    return true;
  }

  /** @returns {Promise<boolean>} */
  async function settleInlineRename() {
    const editor = activeEditor;
    if (!editor) return true;
    return await commitInlineRename(editor, false);
  }

  /**
   * @param {InlineEditor} editor
   * @param {boolean} keepInvalid
   * @returns {Promise<boolean>}
   */
  async function commitInlineRename(editor, keepInvalid) {
    if (activeEditor !== editor) return true;
    if (editor.commitPromise) {
      await editor.commitPromise;
      return activeEditor !== editor;
    }

    const name = editor.input.value;
    if (!isValidInlineName(name)) {
      if (keepInvalid) {
        showInlineError(
          editor,
          "Use one non-empty name without '/' or NUL, at most 255 UTF-8 bytes.",
        );
        editor.input.focus({ preventScroll: true });
        return false;
      }
      await cancelInlineRename(editor, false);
      return true;
    }
    if (name === editor.originalName) {
      await cancelInlineRename(editor, keepInvalid);
      return true;
    }

    const shouldRestoreNameFocus = document.activeElement === editor.input;
    // Install the single in-flight guard before disabling the focused input.
    // Chromium synchronously fires blur when a focused control is disabled;
    // the re-entrant blur handler must observe this promise and never dispatch
    // a second rename.
    editor.commitPromise = Promise.resolve()
      .then(() => onRename(editor.index, name, editor.input))
      .catch(error => {
        showInlineError(editor, `Unable to rename: ${errorMessage(error)}`);
        return /** @type {InlineRenameResult} */ ("retry");
      });
    editor.input.disabled = true;
    editor.input.setAttribute("aria-busy", "true");
    const result = await editor.commitPromise;
    if (activeEditor !== editor) return true;

    if (result === "refresh") {
      await cancelInlineRename(editor, true, true);
      await refreshFromFirstPage();
      return true;
    }
    if (result === "retry") {
      editor.commitPromise = null;
      editor.input.disabled = false;
      editor.input.removeAttribute("aria-busy");
      editor.input.focus({ preventScroll: true });
      const currentItem = items[editor.index];
      placeInlineCaret(
        editor.input,
        currentItem?.path_type.endsWith("Dir") ?? false,
      );
      return false;
    }
    if (result !== "succeeded") {
      await cancelInlineRename(editor, true, true);
      return true;
    }

    const current = items[editor.index];
    if (!current || current.name !== editor.sourceName) {
      notifyMutation(MUTATION_EFFECT.OUTCOME_UNKNOWN);
      await cancelInlineRename(editor, true, true);
      return true;
    }
    loadedNames.delete(current.name);
    const renamedPath = replaceLogicalBasename(current.name, name);
    const renamed = Object.freeze({ ...current, name: renamedPath });
    items.splice(editor.index, 1, renamed);
    loadedNames.add(renamedPath);
    activeEditor = null;
    table.classList.remove("has-inline-editor");
    replaceRenderedRow(editor.index);
    if (shouldRestoreNameFocus) {
      focusName(editor.index);
    } else {
      restoreBlurFocus(editor);
    }
    await refreshAfterInlineRename(editor, renamedPath);
    return true;
  }

  /**
   * @param {InlineEditor} editor
   * @param {boolean} restoreFocus
   * @param {boolean} [keepInvalidated]
   */
  async function cancelInlineRename(
    editor,
    restoreFocus,
    keepInvalidated = false,
  ) {
    if (activeEditor !== editor) return;
    activeEditor = null;
    table.classList.remove("has-inline-editor");
    replaceRenderedRow(editor.index);
    if (restoreFocus) {
      const currentRename = document.getElementById(`renameBtn${editor.index}`);
      focusElement(
        editor.returnFocus?.isConnected
          ? editor.returnFocus
          : currentRename || nameLink(editor.index),
      );
    } else {
      restoreBlurFocus(editor);
    }
    if (keepInvalidated) {
      refreshAfterEditor = false;
    } else if (editor.created || refreshAfterEditor) {
      await refreshAfterInlineRename(editor, editor.sourceName);
    }
  }

  /** @param {InlineEditor} editor @param {string} finalName */
  async function refreshAfterInlineRename(editor, finalName) {
    refreshAfterEditor = false;
    await refreshFromFirstPage();
    if (editor.created && params.q && !matchesFilter(finalName, params.q)) {
      listStatus.textContent =
        `Created "${logicalBasename(finalName)}", but it is hidden by the current filter.`;
    }
  }

  /** @param {InlineEditor} editor @param {string} message */
  function showInlineError(editor, message) {
    if (activeEditor !== editor) return;
    editor.error.textContent = message;
    editor.error.hidden = false;
    editor.input.setAttribute("aria-invalid", "true");
  }

  /** @param {InlineEditor} editor */
  function clearInlineError(editor) {
    if (activeEditor !== editor) return;
    editor.error.textContent = "";
    editor.error.hidden = true;
    editor.input.removeAttribute("aria-invalid");
  }

  /** @param {number} index */
  function replaceRenderedRow(index) {
    const row = document.getElementById(`addPath${index}`);
    const item = items[index];
    if (!(row instanceof HTMLTableRowElement) || !item) return;
    row.replaceWith(createPathRow(item, index));
  }

  /** @param {number} index */
  function focusName(index) {
    focusElement(nameLink(index));
  }

  /** @param {number} index @returns {HTMLAnchorElement | null} */
  function nameLink(index) {
    const link = document.querySelector(`#addPath${index} .cell-name a`);
    return link instanceof HTMLAnchorElement ? link : null;
  }

  /**
   * @param {number} index
   * @param {EventTarget | null} target
   * @returns {InlineEditor["blurFocus"]}
   */
  function captureBlurFocus(index, target) {
    if (!(target instanceof Element)) return null;
    const row = document.getElementById(`addPath${index}`);
    if (!(row instanceof HTMLTableRowElement) || !row.contains(target)) {
      return { element: target };
    }
    const action = target.closest("[data-action]")?.getAttribute("data-action");
    if (["move", "delete", "rename"].includes(action || "")) {
      return /** @type {InlineEditor["blurFocus"]} */ ({ control: action });
    }
    if (target.closest(".cell-actions a")) return { control: "download" };
    return { control: "name" };
  }

  /** @param {InlineEditor} editor */
  function restoreBlurFocus(editor) {
    const target = editor.blurFocus;
    if (!target) return;
    if ("element" in target) {
      focusElement(target.element);
      return;
    }
    const row = document.getElementById(`addPath${editor.index}`);
    const selector = target.control === "name"
      ? ".cell-name a"
      : target.control === "download"
        ? ".cell-actions a"
        : `button[data-action="${target.control}"]`;
    focusElement(row?.querySelector(selector));
  }

  /**
   * @param {ListingItem} file
   * @param {number} index
   * @param {HTMLElement | DocumentFragment} [destination]
   */
  function addPath(file, index, destination = tableBody) {
    destination.append(createPathRow(file, index));
  }

  /**
   * @param {HTMLTableRowElement} row
   * @param {number} index
   */
  function reindexPathRow(row, index) {
    row.id = `addPath${index}`;
    for (const button of row.querySelectorAll(
      "button[data-action][data-index]",
    )) {
      const action = button.getAttribute("data-action");
      if (!action) continue;
      button.id = `${action}Btn${index}`;
      button.setAttribute("data-index", String(index));
    }
  }

  /**
   * @param {HTMLTableRowElement} row
   * @param {number} index
   */
  function pathRowUsesIndex(row, index) {
    const buttons = row.querySelectorAll("button[data-action][data-index]");
    if (buttons.length !== 3) return false;
    return Array.from(buttons).every(button => {
      const action = button.getAttribute("data-action");
      return action !== null &&
        button.id === `${action}Btn${index}` &&
        button.getAttribute("data-index") === String(index);
    });
  }

  /**
   * @param {ListingItem} file
   * @param {number} index
   * @returns {HTMLTableRowElement}
   */
  function createPathRow(file, index) {
    let url = childUrl(file.name);
    const isDir =
      typeof file.path_type === "string" && file.path_type.endsWith("Dir");
    if (isDir) url += "/";

    const row = /** @type {HTMLTableRowElement} */ (createElement("tr", {
      attributes: { id: `addPath${index}` },
    }));
    const iconCell = createElement("td", {
      className: "path cell-icon",
    });
    iconCell.append(getPathIcon(file.path_type));
    const nameCell = createElement("td", {
      className: "path cell-name",
    });
    nameCell.append(createElement("a", {
      text: file.name,
      attributes: {
        href: url,
        download: isDir ? false : true,
      },
    }));

    const actionCell = createElement("td", {
      className: "cell-actions",
    });
    /** @type {HTMLElement | null} */
    let download = null;
    if (!isDir) {
      download = createElement("a", {
        className: "action-btn",
        attributes: {
          href: url,
          title: "Download file",
          "aria-label": `Download file ${file.name}`,
          download: true,
        },
      });
      download.append(createIcon("download"));
    }
    const move = createElement("button", {
      className: "action-btn",
      attributes: {
        id: `moveBtn${index}`,
        type: "button",
        title: "Move",
        "aria-label": `Move ${file.name}`,
        "data-action": "move",
        "data-index": index,
      },
    });
    move.append(createIcon("move"));
    const remove = createElement("button", {
      className: "action-btn",
      attributes: {
        id: `deleteBtn${index}`,
        type: "button",
        title: "Delete",
        "aria-label": `Delete ${file.name}`,
        "data-action": "delete",
        "data-index": index,
      },
    });
    remove.append(createIcon("delete"));
    const rename = createElement("button", {
      className: "action-btn",
      attributes: {
        id: `renameBtn${index}`,
        type: "button",
        title: "Rename",
        "aria-label": `Rename ${file.name}`,
        "data-action": "rename",
        "data-index": index,
      },
    });
    rename.append(createIcon("rename"));
    const actionSlots = createElement("div", {
      className: "action-slots",
    });
    actionSlots.append(
      createActionSlot("move", move),
      createActionSlot("download", download),
      createActionSlot("delete", remove),
      createActionSlot("rename", rename),
    );
    actionCell.append(actionSlots);

    row.append(
      iconCell,
      nameCell,
      createElement("td", {
        className: "cell-mtime",
        text: formatMtime(file.mtime),
      }),
      createElement("td", {
        className: "cell-size",
        text: isDir ? "" : formatFileSize(file.size).join(" "),
      }),
      actionCell,
    );
    return row;
  }

  function setupActions() {
    tableBody.addEventListener("click", event => {
      const target = event.target;
      if (!(target instanceof Element)) return;
      const button = target.closest(
        "button[data-action][data-index]",
      );
      if (!(button instanceof HTMLButtonElement) || !tableBody.contains(button)) {
        return;
      }
      const index = Number(button.dataset.index);
      const action = button.dataset.action;
      if (!Number.isSafeInteger(index) || index < 0 || !action) return;
      if (action === "rename") {
        void startInlineRename(index, button);
        return;
      }
      const name = items[index]?.name;
      if (!name) return;
      void dispatchActionAfterEditor(action, name);
    });
  }

  /** @param {string} action @param {string} name */
  async function dispatchActionAfterEditor(action, name) {
    if (!await settleInlineRename()) return;
    const index = items.findIndex(item => item?.name === name);
    if (index >= 0) onAction(action, index);
  }

  /** @param {() => void | Promise<void>} action */
  async function runListingControl(action) {
    if (await settleInlineRename()) await action();
  }

  /** @param {number} index */
  function remove(index) {
    const row = document.getElementById(`addPath${index}`);
    const focused = document.activeElement;
    const moveFocus = focused instanceof Node && Boolean(row?.contains(focused));
    const focusTarget = /** @type {HTMLElement | null} */ (
      row?.nextElementSibling?.querySelector("button, a") ||
        row?.previousElementSibling?.querySelector("button, a") ||
        document.getElementById("search")
    );
    row?.remove();
    const removed = items[index];
    if (!removed) return;
    items[index] = null;
    loadedNames.delete(removed.name);
    visibleCount--;
    if (index >= renderedStart && index < renderedEnd) {
      renderedVisibleCount = Math.max(0, renderedVisibleCount - 1);
    }
    if (visibleCount === 0 && nextCursor !== null) {
      void loadNextPage();
    } else {
      updateVisibility();
    }
    if (moveFocus) focusTarget?.focus();
  }

  /** @param {string} name */
  function removeByName(name) {
    const index = items.findIndex(item => item?.name === name);
    if (index >= 0) remove(index);
  }

  renderHead();
  setupActions();
  loadMore.addEventListener("click", () => {
    void runListingControl(loadNextPage);
  });
  showPrevious.addEventListener("click", () => {
    void runListingControl(showPreviousWindow);
  });
  showNext.addEventListener("click", () => {
    void runListingControl(showNextWindow);
  });

  return Object.freeze({
    /** @param {number} index */
    getItem(index) {
      return items[index] || null;
    },
    addCreatedItem,
    loadNextPage,
    notifyMutation,
    refreshFromFirstPage,
    remove,
    removeByName,
    settleInlineRename,
    startInlineRename,
    showEmpty() {
      loaded = true;
      nextCursor = null;
      updateVisibility();
    },
  });
}

/** @param {ListingItem} file @returns {Readonly<ListingItem>} */
function validateCreatedItem(file) {
  if (
    !file ||
    !PATH_TYPES.has(file.path_type) ||
    !isValidLogicalPath(file.name) ||
    !Number.isSafeInteger(file.mtime) ||
    file.mtime < 0 ||
    !Number.isSafeInteger(file.size) ||
    file.size < 0 ||
    !isCanonicalRevision(file.revision)
  ) {
    throw new TypeError("Invalid created file list item");
  }
  return Object.freeze({ ...file });
}

/** @param {string} name */
function isValidInlineName(name) {
  return isValidLogicalPath(name) &&
    !name.includes("/") &&
    !name.includes("\0") &&
    new TextEncoder().encode(name).length <= 255;
}

/** @param {string} path */
function logicalBasename(path) {
  return path.slice(path.lastIndexOf("/") + 1);
}

/** @param {string} path @param {string} name */
function replaceLogicalBasename(path, name) {
  const separator = path.lastIndexOf("/");
  return separator < 0 ? name : `${path.slice(0, separator + 1)}${name}`;
}

/** @param {string} name @param {boolean} isDirectory */
function selectionEnd(name, isDirectory) {
  if (isDirectory) return name.length;
  const separator = name.lastIndexOf(".");
  return separator > 0 ? separator : name.length;
}

/** @param {HTMLInputElement} input @param {boolean} isDirectory */
function placeInlineCaret(input, isDirectory) {
  const position = selectionEnd(input.value, isDirectory);
  input.setSelectionRange(position, position);
}

/** @param {string} name @param {string} query */
function matchesFilter(name, query) {
  return name.toLowerCase().includes(query.toLowerCase());
}

/** @param {Element | null | undefined} element */
function focusElement(element) {
  if (element instanceof HTMLElement && element.isConnected) {
    element.focus({ preventScroll: true });
  }
}

/**
 * @param {unknown} payload
 * @returns {{ paths: readonly ListingItem[], nextCursor: string | null }}
 */
function validateListingPage(payload) {
  if (!payload || typeof payload !== "object") {
    throw new Error("Invalid file list response");
  }
  const page = /** @type {Record<string, unknown>} */ (payload);
  const nextCursor = page.next_cursor;
  if (
    !Array.isArray(page.paths) ||
    page.paths.length > LIST_PAGE_LIMIT ||
    !(
      nextCursor === null ||
      (
        typeof nextCursor === "string" &&
        nextCursor.length > 0 &&
        nextCursor.length <= MAX_CURSOR_LENGTH
      )
    )
  ) {
    throw new Error("Invalid file list response");
  }
  const names = new Set();
  const paths = page.paths.map(candidate => {
    if (
      !candidate ||
      typeof candidate !== "object"
    ) {
      throw new Error("Invalid file list item");
    }
    const file = /** @type {Record<string, unknown>} */ (candidate);
    if (
      typeof file.path_type !== "string" ||
      !PATH_TYPES.has(file.path_type) ||
      typeof file.name !== "string" ||
      !isValidLogicalPath(file.name) ||
      file.name.length > 32_768 ||
      typeof file.mtime !== "number" ||
      !Number.isSafeInteger(file.mtime) ||
      file.mtime < 0 ||
      typeof file.size !== "number" ||
      !Number.isSafeInteger(file.size) ||
      file.size < 0 ||
      !isCanonicalRevision(file.revision) ||
      names.has(file.name)
    ) {
      throw new Error("Invalid file list item");
    }
    names.add(file.name);
    return Object.freeze({
      path_type: /** @type {ListingItem["path_type"]} */ (file.path_type),
      name: file.name,
      mtime: file.mtime,
      size: file.size,
      revision: /** @type {string} */ (file.revision),
    });
  });
  return Object.freeze({
    paths: Object.freeze(paths),
    nextCursor,
  });
}

/** @param {unknown} value */
function isCanonicalRevision(value) {
  return typeof value === "string" && /^[0-9a-f]{64}$/.test(value);
}

/** @param {string} text */
function createWindowButton(text) {
  return createElement("button", {
    className: "load-more hidden",
    text,
    attributes: { type: "button" },
  });
}

/** @param {ListingItem["path_type"]} pathType */
function getPathIcon(pathType) {
  switch (pathType) {
    case "Dir":
      return createIcon("dir");
    case "SymlinkFile":
      return createIcon("symlinkFile");
    case "SymlinkDir":
      return createIcon("symlinkDir");
    default:
      return createIcon("file");
  }
}

/** @param {number} mtime */
function formatMtime(mtime) {
  if (!mtime) return "";
  const date = new Date(mtime);
  const year = date.getFullYear();
  const month = padZero(date.getMonth() + 1, 2);
  const day = padZero(date.getDate(), 2);
  const hours = padZero(date.getHours(), 2);
  const minutes = padZero(date.getMinutes(), 2);
  return `${year}-${month}-${day} ${hours}:${minutes}`;
}

/** @param {number} value @param {number} size */
function padZero(value, size) {
  return ("0".repeat(size) + value).slice(-size);
}
