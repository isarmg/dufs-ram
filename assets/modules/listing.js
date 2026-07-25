import { assertResponse, isAuthenticationError } from "./api.js";
import {
  createElement,
  createIcon,
  errorMessage,
  formatFileSize,
} from "./dom.js";
import { childUrl, isValidLogicalPath } from "./path.js";

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
    onUnauthorized,
  } = options;
  let nextCursor = null;
  let loading = false;
  let loaded = false;
  const items = [];

  function renderHead() {
    const headerItems = [
      { name: "name", colspan: 2, text: "名称" },
      { name: "mtime", text: "修改时间" },
      { name: "size", text: "大小" },
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
          "aria-label": `${item.text}，${active ? "切换排序方向" : "按此列排序"}`,
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
      text: "操作",
      attributes: { scope: "col" },
    }));
    tableHead.replaceChildren(row);
  }

  async function loadNextPage() {
    if (loading || (loaded && nextCursor === null)) return;
    const invokedWithFocus = document.activeElement === loadMore;
    loading = true;
    loadMore.textContent = "加载更多";
    loadMore.disabled = true;
    if (!loaded) loadMore.classList.add("hidden");
    listStatus.textContent = loaded ? "正在加载更多…" : "正在加载文件列表…";

    try {
      const url = new URL(
        `${data.uri_prefix}__dufs__/api/list`,
        location.origin,
      );
      url.searchParams.set("path", data.href);
      url.searchParams.set("limit", "200");
      if (params.q) url.searchParams.set("q", params.q);
      if (params.sort) url.searchParams.set("sort", params.sort);
      if (params.order) url.searchParams.set("order", params.order);
      if (nextCursor !== null) url.searchParams.set("cursor", nextCursor);

      const response = await fetch(url);
      if (response.status === 409 && loaded) {
        items.length = 0;
        nextCursor = null;
        loaded = false;
        tableBody.replaceChildren();
        table.classList.add("hidden");
        throw new Error("目录内容已变化，请重新加载列表");
      }
      await assertResponse(response, onUnauthorized);
      const payload = await response.json();
      if (
        !payload ||
        !Array.isArray(payload.paths) ||
        !(payload.next_cursor === null ||
          typeof payload.next_cursor === "string")
      ) {
        throw new Error("文件列表响应格式无效");
      }

      const fragment = document.createDocumentFragment();
      for (const file of payload.paths) {
        const index = items.length;
        items.push(file);
        addPath(file, index, fragment);
      }
      tableBody.append(fragment);
      nextCursor = payload.next_cursor;
      loaded = true;
      updateVisibility(invokedWithFocus);
    } catch (error) {
      if (isAuthenticationError(error)) return;
      listStatus.textContent =
        `无法加载文件列表：${errorMessage(error)}`;
      loadMore.textContent = "重试";
      loadMore.classList.remove("hidden");
      if (invokedWithFocus) loadMore.focus();
    } finally {
      loading = false;
      loadMore.disabled = false;
    }
  }

  function updateVisibility(moveFocusFromLoadMore = false) {
    if (items.some(Boolean)) {
      table.classList.remove("hidden");
      emptyFolder.classList.add("hidden");
    } else {
      table.classList.add("hidden");
      emptyFolder.textContent = emptyNote;
      emptyFolder.classList.remove("hidden");
    }

    if (nextCursor === null) {
      if (moveFocusFromLoadMore) {
        listStatus.tabIndex = -1;
        listStatus.focus();
      }
      loadMore.classList.add("hidden");
      listStatus.textContent = items.some(Boolean)
        ? `已加载全部 ${items.filter(Boolean).length} 项`
        : "";
    } else {
      loadMore.classList.remove("hidden");
      listStatus.textContent =
        `已加载 ${items.filter(Boolean).length} 项`;
    }
  }

  function addPath(file, index, destination = tableBody) {
    if (!file || !isValidLogicalPath(file.name)) {
      const row = createElement("tr", {
        attributes: {
          id: `addPath${index}`,
          "data-invalid-path": "true",
        },
      });
      const iconCell = createElement("td", {
        className: "path cell-icon",
      });
      iconCell.append(createIcon("file"));
      row.append(
        iconCell,
        createElement("td", {
          className: "path cell-name",
          text: "不支持的文件名",
          attributes: { colspan: "4" },
        }),
      );
      destination.append(row);
      return;
    }

    let url = childUrl(file.name);
    const isDir =
      typeof file.path_type === "string" && file.path_type.endsWith("Dir");
    if (isDir) url += "/";

    const row = createElement("tr", {
      attributes: { id: `addPath${index}` },
    });
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
    const download = createElement("a", {
      className: "action-btn",
      attributes: {
        href: isDir ? `${url}?zip` : url,
        title: isDir ? "将文件夹下载为 ZIP" : "下载文件",
        "aria-label": `${isDir ? "下载文件夹" : "下载文件"} ${file.name}`,
        download: true,
      },
    });
    download.append(createIcon("download"));
    const move = createElement("button", {
      className: "action-btn",
      attributes: {
        id: `moveBtn${index}`,
        type: "button",
        title: "移动或重命名",
        "aria-label": `移动或重命名 ${file.name}`,
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
        title: "删除",
        "aria-label": `删除 ${file.name}`,
        "data-action": "delete",
        "data-index": index,
      },
    });
    remove.append(createIcon("delete"));
    actionCell.append(download, move, remove);

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
    destination.append(row);
  }

  function setupActions() {
    tableBody.addEventListener("click", event => {
      const button = event.target.closest(
        "button[data-action][data-index]",
      );
      if (!button || !tableBody.contains(button)) return;
      const index = Number(button.dataset.index);
      if (!Number.isSafeInteger(index) || index < 0) return;
      onAction(button.dataset.action, index);
    });
  }

  function remove(index) {
    const row = document.getElementById(`addPath${index}`);
    const focusTarget =
      row?.nextElementSibling?.querySelector("button, a") ||
      row?.previousElementSibling?.querySelector("button, a") ||
      document.getElementById("search");
    row?.remove();
    items[index] = null;
    if (!items.some(Boolean) && nextCursor !== null) {
      void loadNextPage();
    } else {
      updateVisibility();
    }
    focusTarget?.focus();
  }

  renderHead();
  setupActions();
  loadMore.addEventListener("click", loadNextPage);

  return Object.freeze({
    getItem(index) {
      return items[index] || null;
    },
    loadNextPage,
    remove,
    showEmpty() {
      loaded = true;
      nextCursor = null;
      updateVisibility();
    },
  });
}

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

function padZero(value, size) {
  return ("0".repeat(size) + value).slice(-size);
}
