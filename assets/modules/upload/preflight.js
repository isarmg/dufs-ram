import { isValidLogicalPath } from "../shared/path.js";

/**
 * @typedef {{
 *   path: string,
 *   exists: boolean,
 *   revision: string | null,
 *   replaceable: boolean,
 * }} UploadPreflightTarget
 */

/**
 * Parse a preflight response and bind every result to the exact request path.
 * Order, cardinality and uniqueness are all checked so a partial or reordered
 * response cannot accidentally authorize replacement of a different target.
 *
 * @param {unknown} payload
 * @param {readonly string[]} requestedPaths
 * @returns {readonly Readonly<UploadPreflightTarget>[]}
 */
export function parseUploadPreflight(payload, requestedPaths) {
  if (
    !isRecord(payload) ||
    !Array.isArray(payload.targets) ||
    payload.targets.length !== requestedPaths.length ||
    new Set(requestedPaths).size !== requestedPaths.length ||
    !requestedPaths.every(isAbsoluteLogicalPath)
  ) {
    throw new TypeError("Invalid upload preflight response");
  }

  return Object.freeze(payload.targets.map((value, index) => {
    const expectedPath = requestedPaths[index];
    if (
      !isRecord(value) ||
      value.path !== expectedPath ||
      typeof value.exists !== "boolean" ||
      typeof value.replaceable !== "boolean" ||
      !isRevisionOrNull(value.revision) ||
      (value.exists && value.revision === null) ||
      (!value.exists && value.revision !== null)
    ) {
      throw new TypeError("Invalid upload preflight response");
    }
    return Object.freeze({
      path: expectedPath,
      exists: value.exists,
      revision: value.revision,
      replaceable: value.replaceable,
    });
  }));
}

/** @param {unknown} value @returns {value is Record<string, unknown>} */
function isRecord(value) {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}

/** @param {string} value */
function isAbsoluteLogicalPath(value) {
  return value.startsWith("/") && isValidLogicalPath(value.slice(1));
}

/** @param {unknown} value @returns {value is string | null} */
function isRevisionOrNull(value) {
  return value === null ||
    (typeof value === "string" && /^[0-9a-f]{64}$/.test(value));
}
