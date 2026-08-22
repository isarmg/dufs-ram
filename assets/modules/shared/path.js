export function currentPageUrl() {
  return location.href.split(/[?#]/)[0];
}

/** @param {unknown} name @returns {name is string} */
export function isValidLogicalPath(name) {
  if (typeof name !== "string" || name.length === 0 || name.startsWith("/")) {
    return false;
  }
  return name
    .split("/")
    .every(part => part.length > 0 && part !== "." && part !== "..");
}

/** @param {string} name @param {string} [pageUrl] */
export function childUrl(name, pageUrl = currentPageUrl()) {
  if (!isValidLogicalPath(name)) throw new Error("Invalid path");
  const url = new URL(pageUrl);
  if (!url.pathname.endsWith("/")) url.pathname += "/";
  url.pathname += name.split("/").map(encodeURIComponent).join("/");
  return url.href;
}

/** @param {string} basePath @param {string} name */
export function logicalChildPath(basePath, name) {
  if (!isValidLogicalPath(name)) throw new Error("Invalid path");
  const base = basePath.endsWith("/") ? basePath : `${basePath}/`;
  return `${base}${name}`;
}

/** @param {string} path */
export function browserUrlFromLogicalPath(path) {
  return location.origin + path.split("/").map(encodeURIComponent).join("/");
}
