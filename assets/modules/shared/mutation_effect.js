/**
 * The only outcomes that write-capable UI modules may report to the listing.
 * `NOT_COMMITTED` is intentionally a no-op; the other two invalidate both the
 * rendered rows and the server snapshot cursor before any further pagination.
 *
 * @type {Readonly<{
 *   COMMITTED: "committed",
 *   OUTCOME_UNKNOWN: "outcome-unknown",
 *   NOT_COMMITTED: "not-committed",
 * }>}
 */
export const MUTATION_EFFECT = Object.freeze({
  COMMITTED: "committed",
  OUTCOME_UNKNOWN: "outcome-unknown",
  NOT_COMMITTED: "not-committed",
});
