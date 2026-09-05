import { isErrorEnvelope } from "../../dist/platform.js";

/** Decode only the current platform error contract, never product error aliases.
 * @param {string} text @param {string | null} contentType @returns {string | null}
 */
export function platformErrorCode(text, contentType) {
  if (contentType?.split(";", 1)[0].trim().toLowerCase() !== "application/json") return null;
  try {
    const value = JSON.parse(text);
    return isErrorEnvelope(value) ? value.code : null;
  } catch { return null; }
}
