const INDEX_DATA_KEYS = Object.freeze([
  "href",
  "dir_exists",
  "user",
  "csrf_token",
]);
const USER_UTF8_BYTES_LIMIT = 128;

/**
 * @typedef {Readonly<{
 *   href: string,
 *   dir_exists: boolean,
 *   user: string,
 *   csrf_token: string,
 * }>} IndexData
 */

/**
 * Validate the page context embedded by the server before any UI module uses
 * it. Returning a fresh frozen record prevents later code from retaining
 * unvalidated fields or mutating the trusted bootstrap data.
 *
 * @param {unknown} value
 * @returns {IndexData}
 */
export function parseIndexData(value) {
  if (!isPlainRecord(value)) {
    invalidIndexData("expected a plain object");
  }
  const keys = Reflect.ownKeys(value);
  if (
    keys.length !== INDEX_DATA_KEYS.length ||
    !keys.every(key =>
      typeof key === "string" && INDEX_DATA_KEYS.includes(key)
    )
  ) {
    invalidIndexData("expected exactly href, dir_exists, user, and csrf_token");
  }

  const href = ownDataValue(value, "href");
  const dirExists = ownDataValue(value, "dir_exists");
  const user = ownDataValue(value, "user");
  const csrfToken = ownDataValue(value, "csrf_token");

  if (!isCanonicalAbsoluteLogicalPath(href)) {
    invalidIndexData("href must be a canonical absolute logical path");
  }
  if (typeof dirExists !== "boolean") {
    invalidIndexData("dir_exists must be a boolean");
  }
  if (
    typeof user !== "string" ||
    new TextEncoder().encode(user).byteLength > USER_UTF8_BYTES_LIMIT
  ) {
    invalidIndexData(
      `user must be a string no longer than ${USER_UTF8_BYTES_LIMIT} UTF-8 bytes`,
    );
  }
  if (
    typeof csrfToken !== "string" ||
    !/^[0-9a-f]{64}$/u.test(csrfToken)
  ) {
    invalidIndexData("csrf_token must be 64 lowercase hexadecimal characters");
  }

  return Object.freeze({
    href,
    dir_exists: dirExists,
    user,
    csrf_token: csrfToken,
  });
}

/** @param {unknown} value @returns {value is Record<string, unknown>} */
function isPlainRecord(value) {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    return false;
  }
  const prototype = Object.getPrototypeOf(value);
  return prototype === Object.prototype || prototype === null;
}

/**
 * @param {Record<string, unknown>} record
 * @param {string} key
 * @returns {unknown}
 */
function ownDataValue(record, key) {
  const descriptor = Object.getOwnPropertyDescriptor(record, key);
  if (!descriptor || !Object.hasOwn(descriptor, "value")) {
    invalidIndexData(`${key} must be a data property`);
  }
  return descriptor.value;
}

/** @param {unknown} value @returns {value is string} */
function isCanonicalAbsoluteLogicalPath(value) {
  if (
    typeof value !== "string" ||
    !value.startsWith("/") ||
    value.includes("\0")
  ) {
    return false;
  }
  if (value === "/") return true;
  return value.slice(1).split("/").every(
    part => part.length > 0 && part !== "." && part !== "..",
  );
}

/** @param {string} reason @returns {never} */
function invalidIndexData(reason) {
  throw new TypeError(`Invalid embedded index data: ${reason}`);
}
