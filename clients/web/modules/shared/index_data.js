import { isAdministratorSession } from "../../dist/platform.js";

/**
 * @typedef {Readonly<{
 *   href: string,
 *   dir_exists: boolean,
 *   session: Readonly<import("@sarmg/contracts").AdministratorSession>,
 * }>} IndexData
 */

/**
 * Page metadata contains only business context. Session data must come from
 * the shared client's restore endpoint, never an HTML bootstrap field.
 * @param {unknown} value
 * @param {unknown} session
 * @returns {IndexData}
 */
export function parseIndexData(value, session) {
  if (!isPlainRecord(value)) invalidIndexData("expected a plain object");
  const keys = Reflect.ownKeys(value);
  if (keys.length !== 2 || !keys.every(key => key === "href" || key === "dir_exists")) {
    invalidIndexData("expected exactly href and dir_exists");
  }
  const href = ownDataValue(value, "href");
  const dirExists = ownDataValue(value, "dir_exists");
  if (!isCanonicalAbsoluteLogicalPath(href)) invalidIndexData("href must be a canonical absolute logical path");
  if (typeof dirExists !== "boolean") invalidIndexData("dir_exists must be a boolean");
  if (!isAdministratorSession(session)) invalidIndexData("session must satisfy the Foundation contract");
  return Object.freeze({ href, dir_exists: dirExists, session: Object.freeze({ ...session }) });
}

/** @param {unknown} value @returns {value is Record<string, unknown>} */
function isPlainRecord(value) {
  if (value === null || typeof value !== "object" || Array.isArray(value)) return false;
  const prototype = Object.getPrototypeOf(value);
  return prototype === Object.prototype || prototype === null;
}

/** @param {Record<string, unknown>} record @param {string} key @returns {unknown} */
function ownDataValue(record, key) {
  const descriptor = Object.getOwnPropertyDescriptor(record, key);
  if (!descriptor || !Object.hasOwn(descriptor, "value")) invalidIndexData(`${key} must be a data property`);
  return descriptor.value;
}

/** @param {unknown} value @returns {value is string} */
function isCanonicalAbsoluteLogicalPath(value) {
  if (typeof value !== "string" || !value.startsWith("/") || value.includes("\0")) return false;
  return value === "/" || value.slice(1).split("/").every(part => part.length > 0 && part !== "." && part !== "..");
}

/** @param {string} reason @returns {never} */
function invalidIndexData(reason) {
  throw new TypeError(`Invalid embedded index data: ${reason}`);
}
