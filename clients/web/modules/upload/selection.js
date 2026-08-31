import { isValidLogicalPath } from "../shared/path.js";

export const UPLOAD_BATCH_FILE_LIMIT = 512;
export const UPLOAD_BATCH_PATH_BYTES_LIMIT = 256 * 1024;

/** @typedef {{ file: File, name: string }} UploadSelectionEntry */

/**
 * Validate a selection before creating upload state or DOM.
 *
 * @param {FileList | File[] | null | undefined} files
 * @param {{ fileLimit?: number, pathBytesLimit?: number }} [options]
 * @returns {{
 *   ok: boolean,
 *   error: string,
 *   entries: readonly UploadSelectionEntry[],
 *   totalPathBytes: number,
 * }}
 */
export function prepareUploadSelection(files, options = {}) {
  const fileLimit = options.fileLimit ?? UPLOAD_BATCH_FILE_LIMIT;
  const pathBytesLimit = options.pathBytesLimit ??
    UPLOAD_BATCH_PATH_BYTES_LIMIT;
  if (!Number.isSafeInteger(fileLimit) || fileLimit <= 0) {
    throw new TypeError("Upload file limit must be a positive integer");
  }
  if (!Number.isSafeInteger(pathBytesLimit) || pathBytesLimit <= 0) {
    throw new TypeError("Upload path byte limit must be a positive integer");
  }

  /** @type {UploadSelectionEntry[]} */
  const entries = [];
  const encoder = new TextEncoder();
  let totalPathBytes = 0;
  const length = files?.length || 0;
  for (let index = 0; index < length; index++) {
    if (entries.length >= fileLimit) {
      return Object.freeze({
        ok: false,
        error:
          `Select no more than ${fileLimit} files in one batch. ` +
          "Split larger folders into multiple selections.",
        entries: Object.freeze([]),
        totalPathBytes,
      });
    }
    const file = files?.[index];
    if (!(file instanceof File)) {
      return Object.freeze({
        ok: false,
        error: "The browser returned an invalid file selection.",
        entries: Object.freeze([]),
        totalPathBytes,
      });
    }
    const name = file.webkitRelativePath || file.name;
    if (
      !isValidLogicalPath(name) ||
      name.split("/").at(-1) !== file.name
    ) {
      return Object.freeze({
        ok: false,
        error: `The selected path ${name || "(empty)"} is not supported.`,
        entries: Object.freeze([]),
        totalPathBytes,
      });
    }
    totalPathBytes += encoder.encode(name).byteLength;
    if (totalPathBytes > pathBytesLimit) {
      return Object.freeze({
        ok: false,
        error:
          `Selected paths exceed the ${pathBytesLimit}-byte batch limit. ` +
          "Split the selection into smaller batches.",
        entries: Object.freeze([]),
        totalPathBytes,
      });
    }
    entries.push(Object.freeze({ file, name }));
  }
  return Object.freeze({
    ok: true,
    error: "",
    entries: Object.freeze(entries),
    totalPathBytes,
  });
}
