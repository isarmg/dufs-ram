import { createActionDialogs } from "./operations/dialogs.js";
import { createFileOperations } from "./operations/file_operations.js";
import { createDirectoryListing } from "./listing/controller.js";
import { createElement, createIcon, errorMessage } from "./shared/dom.js";
// Dufs is the suite's explicit React/Vite migration exception: this client
// remains a browser-native ES-module application with no framework bootstrap.
import { parseIndexData } from "./shared/index_data.js";
import { currentPageUrl } from "./shared/path.js";
import { createUploadManager } from "./upload/manager.js";

/** @type {ReturnType<typeof parseIndexData>} */
let data;
/** @type {ReturnType<typeof createDirectoryListing>} */
let directoryListing;
/** @type {ReturnType<typeof createFileOperations>} */
let fileOperations;
/** @type {ReturnType<typeof createUploadManager>} */
let uploadManager;
let redirectingToLogin = false;

const searchParams = new URLSearchParams(window.location.search);
const requestedSort = searchParams.get("sort") || "";
const requestedOrder = searchParams.get("order") || "";
const params = Object.freeze({
  q: searchParams.get("q") || "",
  sort: ["name", "mtime", "size"].includes(requestedSort)
    ? requestedSort
    : "name",
  order:
    ["name", "mtime", "size"].includes(requestedSort) &&
      ["asc", "desc"].includes(requestedOrder)
      ? requestedOrder
      : "asc",
});

export function start() {
  window.addEventListener("DOMContentLoaded", () => {
    void initialize().catch(error => {
      console.error(error);
      showFatalError(`Unable to initialize the file manager: ${errorMessage(error)}`);
    });
  });
}

async function initialize() {
  const indexData = /** @type {HTMLTemplateElement | null} */ (
    document.getElementById("index-data")
  );
  if (!indexData) throw new Error("Page data is missing");
  /** @type {unknown} */
  const rawData = JSON.parse(decodeBase64(indexData.content.textContent));
  data = parseIndexData(rawData);
  addBreadcrumb(data.href);
  document.title = `${data.href} - Dufs File Manager`;

  const pathsTable = requiredElement(".paths-table", HTMLTableElement);
  const pathsTableBody = requiredElement(
    ".paths-table tbody",
    HTMLTableSectionElement,
  );
  const emptyFolder = requiredElement(".empty-folder", HTMLElement);
  const dialogs = createActionDialogs();
  const emptyNote = params.q
    ? "No search results"
    : data.dir_exists
      ? "Folder is empty"
      : "Uploading files will create this folder automatically";

  directoryListing = createDirectoryListing({
    data,
    params,
    table: pathsTable,
    tableHead: requiredElement(
      ".paths-table thead",
      HTMLTableSectionElement,
    ),
    tableBody: pathsTableBody,
    emptyFolder,
    emptyNote,
    loadMore: requiredElement(".load-more", HTMLButtonElement),
    listStatus: requiredElement(".list-status", HTMLElement),
    onAction(action, index) {
      if (action === "move") {
        void fileOperations.movePath(index);
      } else if (action === "delete") {
        void fileOperations.deletePath(index);
      }
    },
    onRename(index, name, returnFocus) {
      return fileOperations.renamePath(index, name, returnFocus);
    },
    onUnauthorized: redirectToLogin,
  });
  fileOperations = createFileOperations({
    data,
    listing: directoryListing,
    dialogs,
    operationStatus: requiredElement(".operation-status", HTMLElement),
    onUnauthorized: redirectToLogin,
  });
  uploadManager = createUploadManager({
    data,
    dialogs,
    uploadersTable: requiredElement(".uploaders-table", HTMLTableElement),
    queueMessage: requiredElement(".upload-queue-message", HTMLElement),
    historyStatus: requiredElement(".upload-history-status", HTMLElement),
    emptyFolder,
    onMutation: effect => directoryListing.notifyMutation(effect),
    onUnauthorized: redirectToLogin,
  });

  setupFileDropGuard();
  setupUploadFile();
  setupUploadFolder();
  setupNewFolder();
  setupNewFile();
  setupAuth();
  setupSearch();

  requiredElement(".index-page", HTMLElement).classList.remove("hidden");
  if (data.dir_exists) {
    await directoryListing.loadNextPage();
  } else {
    directoryListing.showEmpty();
  }
}

/**
 * @template {Element} T
 * @param {string} selector
 * @param {{ new (...args: never[]): T }} constructor
 * @returns {T}
 */
function requiredElement(selector, constructor) {
  const element = document.querySelector(selector);
  if (!element) throw new Error(`Required page control is missing: ${selector}`);
  if (constructor && !(element instanceof constructor)) {
    throw new Error(`Required page control has the wrong type: ${selector}`);
  }
  return /** @type {T} */ (element);
}

/** @param {string} message */
function showFatalError(message) {
  document.querySelector(".index-page")?.classList.remove("hidden");
  const status = document.querySelector(".list-status");
  if (status) {
    status.setAttribute("role", "alert");
    status.textContent = message;
    return;
  }
  document.body.append(createElement("p", {
    text: message,
    attributes: { role: "alert" },
  }));
}

/** @param {string} href */
function addBreadcrumb(href) {
  const breadcrumb = requiredElement(".breadcrumb", HTMLElement);
  const parts = href === "/" ? [""] : href.split("/");
  let path = "/";
  for (let index = 0; index < parts.length; index++) {
    const name = parts[index];
    if (index > 0) {
      if (!path.endsWith("/")) path += "/";
      path += encodeURIComponent(name);
    }
    if (index === 0) {
      const root = createElement("a", {
        attributes: {
          href: path,
          title: "Root",
          "aria-label": "Root",
        },
      });
      root.append(createIcon("home"));
      breadcrumb.append(root);
    } else if (index === parts.length - 1) {
      breadcrumb.append(createElement("b", { text: name }));
    } else {
      breadcrumb.append(createElement("a", {
        text: name,
        attributes: { href: path },
      }));
    }
    if (index !== parts.length - 1) {
      breadcrumb.append(createElement("span", {
        className: "separator",
        text: "/",
        attributes: { "aria-hidden": "true" },
      }));
    }
  }
}

function setupFileDropGuard() {
  for (const name of ["dragover", "drop"]) {
    document.addEventListener(name, event => {
      const dataTransfer = /** @type {DragEvent} */ (event).dataTransfer;
      const types = Array.from(dataTransfer?.types || []);
      if (!dataTransfer || !types.includes("Files")) return;
      event.preventDefault();
      dataTransfer.dropEffect = "none";
    });
  }
}

function setupAuth() {
  const logout = requiredElement(".logout-btn", HTMLButtonElement);
  requiredElement(".user-name", HTMLElement).textContent = data.session.username;
  logout.classList.remove("hidden");
  logout.addEventListener("click", () => {
    void fileOperations.logout();
  });
}

function setupSearch() {
  const searchbar = requiredElement(".searchbar", HTMLFormElement);
  searchbar.classList.remove("hidden");
  searchbar.addEventListener("submit", event => {
    event.preventDefault();
    const query = new FormData(searchbar).get("q");
    const url = new URL(currentPageUrl());
    if (typeof query === "string" && query) url.searchParams.set("q", query);
    location.href = url.toString();
  });
  if (params.q) {
    requiredElement("#search", HTMLInputElement).value = params.q;
  }
}

function setupUploadFile() {
  const button = requiredElement(".upload-file", HTMLButtonElement);
  const input = requiredElement("#file", HTMLInputElement);
  button.classList.remove("hidden");
  button.addEventListener("click", () => input.click());
  input.addEventListener("change", () => {
    void uploadManager.addFiles(input.files, { returnFocus: button });
    input.value = "";
  });
}

function setupUploadFolder() {
  const button = requiredElement(".upload-folder", HTMLButtonElement);
  const input = requiredElement("#folder", HTMLInputElement);
  button.classList.remove("hidden");
  button.addEventListener("click", () => input.click());
  input.addEventListener("change", () => {
    void uploadManager.addFiles(input.files, { returnFocus: button });
    input.value = "";
  });
}

function setupNewFolder() {
  const button = requiredElement(".new-folder", HTMLButtonElement);
  button.classList.remove("hidden");
  button.addEventListener("click", () => {
    void fileOperations.createDefaultFolder(button);
  });
}

function setupNewFile() {
  const button = requiredElement(".new-file", HTMLButtonElement);
  button.classList.remove("hidden");
  button.addEventListener("click", () => {
    void fileOperations.createDefaultFile(button);
  });
}

function redirectToLogin() {
  if (redirectingToLogin) return;
  redirectingToLogin = true;
  // A running upload can raise a beforeunload confirmation. If the user
  // cancels that navigation, this document remains active and must be able to
  // react to a later authentication failure. A successful reload destroys the
  // document before this reset matters.
  window.setTimeout(() => {
    redirectingToLogin = false;
  }, 0);
  location.reload();
}

/** @param {string} base64String */
function decodeBase64(base64String) {
  const binary = atob(base64String);
  const bytes = Uint8Array.from(
    binary,
    character => character.charCodeAt(0),
  );
  return new TextDecoder().decode(bytes);
}
