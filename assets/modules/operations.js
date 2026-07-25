import {
  CSRF_HEADER,
  assertResponse,
  isAuthenticationError,
  postJson,
} from "./api.js";
import { errorMessage } from "./dom.js";
import {
  browserUrlFromLogicalPath,
  childUrl,
  isValidLogicalPath,
  logicalChildPath,
} from "./path.js";

export function createFileOperations(options) {
  const {
    data,
    listing,
    onUnauthorized,
  } = options;
  const pending = new Set();

  async function deletePath(index) {
    const file = listing.getItem(index);
    if (!file || !isValidLogicalPath(file.name)) return;
    const pendingKey = `path:${index}`;
    if (pending.has(pendingKey)) return;
    if (!confirm(`确定删除“${file.name}”吗？`)) return;
    if (!begin(pendingKey)) return;
    try {
      const response = await fetch(childUrl(file.name), {
        method: "DELETE",
        headers: {
          [CSRF_HEADER]: data.csrf_token,
        },
      });
      await assertResponse(response, onUnauthorized);
      listing.remove(index);
    } catch (error) {
      if (isAuthenticationError(error)) return;
      alert(`无法删除“${file.name}”：${errorMessage(error)}`);
    } finally {
      pending.delete(pendingKey);
    }
  }

  async function movePath(index) {
    const file = listing.getItem(index);
    if (!file || !isValidLogicalPath(file.name)) return;
    const pendingKey = `path:${index}`;
    if (pending.has(pendingKey)) return;
    const source = logicalPath(file.name);
    let destination = prompt("请输入新路径", source);
    if (!destination) return;
    if (!destination.startsWith("/")) destination = `/${destination}`;
    if (source === destination) return;
    if (!begin(pendingKey)) return;

    try {
      const request = {
        source,
        destination,
        overwrite: false,
      };
      let response = await postBrowserApi("move", request);
      if (
        response.status === 409 &&
        await response.clone().text() === "Destination already exists"
      ) {
        if (!confirm("目标文件已存在，确定覆盖吗？")) return;
        request.overwrite = true;
        response = await postBrowserApi("move", request);
      }
      await assertResponse(response, onUnauthorized);
      const targetUrl = browserUrlFromLogicalPath(
        data.uri_prefix,
        destination,
      );
      location.href = targetUrl.split("/").slice(0, -1).join("/");
    } catch (error) {
      if (isAuthenticationError(error)) return;
      alert(
        `无法将“${source}”移动到“${destination}”：${errorMessage(error)}`,
      );
    } finally {
      pending.delete(pendingKey);
    }
  }

  async function logout() {
    if (!begin("logout")) return;
    try {
      const response = await fetch(
        `${data.uri_prefix}__dufs__/logout`,
        {
          method: "POST",
          headers: {
            [CSRF_HEADER]: data.csrf_token,
          },
        },
      );
      await assertResponse(response, onUnauthorized);
      onUnauthorized();
    } catch (error) {
      if (isAuthenticationError(error)) return;
      alert(`无法退出登录：${errorMessage(error)}`);
    } finally {
      pending.delete("logout");
    }
  }

  async function createFolder(name) {
    if (!begin("create-folder")) return;
    try {
      const response = await postBrowserApi("mkdir", {
        path: logicalPath(name),
      });
      await assertResponse(response, onUnauthorized);
      location.href = childUrl(name);
    } catch (error) {
      if (isAuthenticationError(error)) return;
      alert(`无法创建文件夹“${name}”：${errorMessage(error)}`);
    } finally {
      pending.delete("create-folder");
    }
  }

  async function createFile(name) {
    if (!begin("create-file")) return;
    try {
      const response = await fetch(childUrl(name), {
        method: "PUT",
        headers: {
          [CSRF_HEADER]: data.csrf_token,
          "X-Dufs-Upload-Id": crypto.randomUUID(),
          "X-Dufs-Upload-Length": "0",
        },
        body: "",
      });
      await assertResponse(response, onUnauthorized);
      location.reload();
    } catch (error) {
      if (isAuthenticationError(error)) return;
      alert(`无法创建文件“${name}”：${errorMessage(error)}`);
    } finally {
      pending.delete("create-file");
    }
  }

  function begin(key) {
    if (pending.has(key)) return false;
    pending.add(key);
    return true;
  }

  function logicalPath(name) {
    return logicalChildPath(data.href, name);
  }

  function postBrowserApi(action, body) {
    return postJson(
      `${data.uri_prefix}__dufs__/api/${action}`,
      data.csrf_token,
      body,
    );
  }

  return Object.freeze({
    createFile,
    createFolder,
    deletePath,
    logout,
    movePath,
  });
}
