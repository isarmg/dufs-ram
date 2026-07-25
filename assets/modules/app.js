import { createElement, errorMessage } from "./dom.js";
import { createDirectoryListing } from "./listing.js";
import { createFileOperations } from "./operations.js";
import { currentPageUrl } from "./path.js";
import { createUploadManager } from "./upload.js";

let data;
let directoryListing;
let fileOperations;
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
      showFatalError(`无法初始化文件管理器：${errorMessage(error)}`);
    });
  });
}

async function initialize() {
  const indexData = document.getElementById("index-data");
  if (!indexData) throw new Error("页面数据缺失");
  data = JSON.parse(decodeBase64(indexData.content.textContent));
  addBreadcrumb(data.href, data.uri_prefix);
  document.title = `${data.href} - Dufs 文件管理器`;

  const pathsTable = requiredElement(".paths-table");
  const pathsTableBody = requiredElement(".paths-table tbody");
  const emptyFolder = requiredElement(".empty-folder");
  const emptyNote = params.q
    ? "没有搜索结果"
    : data.dir_exists
      ? "文件夹为空"
      : "上传文件时将自动创建此文件夹";

  directoryListing = createDirectoryListing({
    data,
    params,
    table: pathsTable,
    tableHead: requiredElement(".paths-table thead"),
    tableBody: pathsTableBody,
    emptyFolder,
    emptyNote,
    loadMore: requiredElement(".load-more"),
    listStatus: requiredElement(".list-status"),
    onAction(action, index) {
      if (action === "move") {
        void fileOperations.movePath(index);
      } else if (action === "delete") {
        void fileOperations.deletePath(index);
      }
    },
    onUnauthorized: redirectToLogin,
  });
  fileOperations = createFileOperations({
    data,
    listing: directoryListing,
    onUnauthorized: redirectToLogin,
  });
  uploadManager = createUploadManager({
    data,
    uploadersTable: requiredElement(".uploaders-table"),
    queueMessage: requiredElement(".upload-queue-message"),
    emptyFolder,
    onUnauthorized: redirectToLogin,
  });

  setupDownload();
  setupFileDropGuard();
  setupUploadFile();
  setupUploadFolder();
  setupNewFolder();
  setupNewFile();
  setupAuth();
  setupSearch();

  requiredElement(".index-page").classList.remove("hidden");
  if (data.dir_exists) {
    await directoryListing.loadNextPage();
  } else {
    directoryListing.showEmpty();
  }
}

function requiredElement(selector) {
  const element = document.querySelector(selector);
  if (!element) throw new Error(`页面控件缺失：${selector}`);
  return element;
}

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

function addBreadcrumb(href, uriPrefix) {
  const breadcrumb = requiredElement(".breadcrumb");
  const parts = href === "/" ? [""] : href.split("/");
  let path = uriPrefix;
  for (let index = 0; index < parts.length; index++) {
    const name = parts[index];
    if (index > 0) {
      if (!path.endsWith("/")) path += "/";
      path += encodeURIComponent(name);
    }
    if (index === 0) {
      breadcrumb.append(createElement("a", {
        text: "根目录",
        attributes: {
          href: path,
          title: "根目录",
        },
      }));
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

function setupDownload() {
  if (!data.dir_exists) return;
  const download = requiredElement(".download");
  download.href = `${currentPageUrl()}?zip`;
  download.title = "将当前文件夹下载为 ZIP";
  download.setAttribute("aria-label", "将当前文件夹下载为 ZIP");
  download.classList.remove("hidden");
}

function setupFileDropGuard() {
  for (const name of ["dragover", "drop"]) {
    document.addEventListener(name, event => {
      const types = Array.from(event.dataTransfer?.types || []);
      if (!types.includes("Files")) return;
      event.preventDefault();
      event.dataTransfer.dropEffect = "none";
    });
  }
}

function setupAuth() {
  const logout = requiredElement(".logout-btn");
  requiredElement(".user-name").textContent = data.user;
  logout.classList.remove("hidden");
  logout.addEventListener("click", () => {
    void fileOperations.logout();
  });
}

function setupSearch() {
  const searchbar = requiredElement(".searchbar");
  searchbar.classList.remove("hidden");
  searchbar.addEventListener("submit", event => {
    event.preventDefault();
    const query = new FormData(searchbar).get("q");
    const url = new URL(currentPageUrl());
    if (query) url.searchParams.set("q", query);
    location.href = url.toString();
  });
  if (params.q) requiredElement("#search").value = params.q;
}

function setupUploadFile() {
  const button = requiredElement(".upload-file");
  const input = requiredElement("#file");
  button.classList.remove("hidden");
  button.addEventListener("click", () => input.click());
  input.addEventListener("change", event => {
    uploadManager.addFiles(event.target.files);
    event.target.value = "";
  });
}

function setupUploadFolder() {
  const button = requiredElement(".upload-folder");
  const input = requiredElement("#folder");
  button.classList.remove("hidden");
  button.addEventListener("click", () => input.click());
  input.addEventListener("change", event => {
    uploadManager.addFiles(event.target.files);
    event.target.value = "";
  });
}

function setupNewFolder() {
  const button = requiredElement(".new-folder");
  button.classList.remove("hidden");
  button.addEventListener("click", () => {
    const name = prompt("请输入文件夹名称");
    if (name) void fileOperations.createFolder(name);
  });
}

function setupNewFile() {
  const button = requiredElement(".new-file");
  button.classList.remove("hidden");
  button.addEventListener("click", () => {
    const name = prompt("请输入文件名称");
    if (name) void fileOperations.createFile(name);
  });
}

function redirectToLogin() {
  if (redirectingToLogin) return;
  redirectingToLogin = true;
  location.reload();
}

function decodeBase64(base64String) {
  const binary = atob(base64String);
  const bytes = Uint8Array.from(
    binary,
    character => character.charCodeAt(0),
  );
  return new TextDecoder().decode(bytes);
}
