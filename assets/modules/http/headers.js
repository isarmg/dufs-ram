/**
 * Parse one canonical non-negative integer HTTP header value without accepting
 * signs, whitespace, leading zeroes, fractions, exponents or unsafe integers.
 *
 * @param {string | null} value
 * @returns {number | null}
 */
export function parseUnsignedHeader(value) {
  if (typeof value !== "string" || !/^(0|[1-9][0-9]*)$/.test(value)) {
    return null;
  }
  const parsed = Number(value);
  return Number.isSafeInteger(parsed) ? parsed : null;
}
