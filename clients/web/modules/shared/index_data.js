const INDEX_DATA_KEYS = Object.freeze([
  "href",
  "dir_exists",
  "session",
]);
const SESSION_KEYS = Object.freeze([
  "authenticated",
  "user_id",
  "username",
  "role",
  "csrf_token",
]);
const CANONICAL_ADMINISTRATOR_USERNAME_PATTERN =
  /^(?=.{3,64}$)[a-z0-9][a-z0-9._-]*[a-z0-9]$/u;
const AUTHENTICATION_TOKEN_PATTERN =
  /^[A-Za-z0-9_-]{42}[AEIMQUYcgkosw048]$/u;

/**
 * @typedef {Readonly<{
 *   href: string,
 *   dir_exists: boolean,
 *   session: Readonly<{
 *     authenticated: true,
 *     user_id: string,
 *     username: string,
 *     role: "admin",
 *     csrf_token: string,
 *   }>,
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
    invalidIndexData("expected exactly href, dir_exists, and session");
  }

  const href = ownDataValue(value, "href");
  const dirExists = ownDataValue(value, "dir_exists");
  const session = parseAdministratorSession(ownDataValue(value, "session"));

  if (!isCanonicalAbsoluteLogicalPath(href)) {
    invalidIndexData("href must be a canonical absolute logical path");
  }
  if (typeof dirExists !== "boolean") {
    invalidIndexData("dir_exists must be a boolean");
  }
  return Object.freeze({
    href,
    dir_exists: dirExists,
    session,
  });
}

/**
 * Enforce the one current Foundation administrator-session contract before
 * any product module receives authentication data. Dufs also enforces the
 * Foundation 256-bit URL-safe token representation at this trust boundary.
 *
 * @param {unknown} value
 * @returns {IndexData["session"]}
 */
function parseAdministratorSession(value) {
  if (!isPlainRecord(value)) {
    invalidIndexData("session must be a plain object");
  }
  const keys = Reflect.ownKeys(value);
  if (
    keys.length !== SESSION_KEYS.length ||
    !keys.every(key => typeof key === "string" && SESSION_KEYS.includes(key))
  ) {
    invalidIndexData(
      "session must contain exactly authenticated, user_id, username, role, and csrf_token",
    );
  }

  const authenticated = ownDataValue(value, "authenticated");
  const userId = ownDataValue(value, "user_id");
  const username = ownDataValue(value, "username");
  const role = ownDataValue(value, "role");
  const csrfToken = ownDataValue(value, "csrf_token");
  if (authenticated !== true) {
    invalidIndexData("session.authenticated must be true");
  }
  if (typeof userId !== "string" || !/^[A-Za-z0-9._:-]{1,128}$/u.test(userId)) {
    invalidIndexData("session.user_id must be a Foundation identifier");
  }
  if (!isCanonicalAdministratorUsername(username)) {
    invalidIndexData("session.username must be a canonical administrator username");
  }
  if (role !== "admin") {
    invalidIndexData("session.role must be admin");
  }
  if (typeof csrfToken !== "string" || !AUTHENTICATION_TOKEN_PATTERN.test(csrfToken)) {
    invalidIndexData("session.csrf_token must be a Foundation URL-safe token");
  }
  return Object.freeze({
    authenticated: true,
    user_id: userId,
    username,
    role: "admin",
    csrf_token: csrfToken,
  });
}

/** @param {unknown} value @returns {value is string} */
function isCanonicalAdministratorUsername(value) {
  return typeof value === "string" &&
    CANONICAL_ADMINISTRATOR_USERNAME_PATTERN.test(value);
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
