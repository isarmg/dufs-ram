export function currentPageUrl() {
  return location.href.split(/[?#]/)[0];
}

export function isValidLogicalPath(name) {
  if (typeof name !== "string" || name.length === 0 || name.startsWith("/")) {
    return false;
  }
  return name
    .split("/")
    .every(part => part.length > 0 && part !== "." && part !== "..");
}

export function childUrl(name, pageUrl = currentPageUrl()) {
  if (!isValidLogicalPath(name)) throw new Error("无效路径");
  const url = new URL(pageUrl);
  if (!url.pathname.endsWith("/")) url.pathname += "/";
  url.pathname += name.split("/").map(encodeURIComponent).join("/");
  return url.href;
}

export function logicalChildPath(basePath, name) {
  if (!isValidLogicalPath(name)) throw new Error("无效路径");
  const base = basePath.endsWith("/") ? basePath : `${basePath}/`;
  return `${base}${name}`;
}

export function browserUrlFromLogicalPath(uriPrefix, path) {
  const prefix = uriPrefix.slice(0, -1);
  return location.origin + prefix + path.split("/").map(encodeURIComponent).join("/");
}
