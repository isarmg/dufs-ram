const SVG_NS = "http://www.w3.org/2000/svg";

/** @typedef {{ d: string, fillRule?: string }} IconPath */
/** @typedef {{ width: number, viewBox: string, paths: readonly IconPath[] }} IconDefinition */

/** @type {Readonly<Record<string, IconDefinition>>} */
const ICONS = Object.freeze({
  home: {
    width: 16,
    viewBox: "0 0 16 16",
    paths: [{
      d: "M6.5 14.5v-3.505c0-.245.25-.495.5-.495h2c.25 0 .5.25.5.5v3.5a.5.5 0 0 0 .5.5h4a.5.5 0 0 0 .5-.5v-7a.5.5 0 0 0-.146-.354L13 5.793V2.5a.5.5 0 0 0-.5-.5h-1a.5.5 0 0 0-.5.5v1.293L8.354 1.146a.5.5 0 0 0-.708 0l-6 6A.5.5 0 0 0 1.5 7.5v7a.5.5 0 0 0 .5.5h4a.5.5 0 0 0 .5-.5z",
    }],
  },
  dir: {
    width: 14,
    viewBox: "0 0 14 16",
    paths: [{
      fillRule: "evenodd",
      d: "M13 4H7V3c0-.66-.31-1-1-1H1c-.55 0-1 .45-1 1v10c0 .55.45 1 1 1h12c.55 0 1-.45 1-1V5c0-.55-.45-1-1-1zM6 4H1V3h5v1z",
    }],
  },
  symlinkFile: {
    width: 12,
    viewBox: "0 0 12 16",
    paths: [{
      fillRule: "evenodd",
      d: "M8.5 1H1c-.55 0-1 .45-1 1v12c0 .55.45 1 1 1h10c.55 0 1-.45 1-1V4.5L8.5 1zM11 14H1V2h7l3 3v9zM6 4.5l4 3-4 3v-2c-.98-.02-1.84.22-2.55.7-.71.48-1.19 1.25-1.45 2.3.02-1.64.39-2.88 1.13-3.73.73-.84 1.69-1.27 2.88-1.27v-2H6z",
    }],
  },
  symlinkDir: {
    width: 14,
    viewBox: "0 0 14 16",
    paths: [{
      fillRule: "evenodd",
      d: "M13 4H7V3c0-.66-.31-1-1-1H1c-.55 0-1 .45-1 1v10c0 .55.45 1 1 1h12c.55 0 1-.45 1-1V5c0-.55-.45-1-1-1zM1 3h5v1H1V3zm6 9v-2c-.98-.02-1.84.22-2.55.7-.71.48-1.19 1.25-1.45 2.3.02-1.64.39-2.88 1.13-3.73C4.86 8.43 5.82 8 7.01 8V6l4 3-4 3H7z",
    }],
  },
  file: {
    width: 12,
    viewBox: "0 0 12 16",
    paths: [{
      fillRule: "evenodd",
      d: "M6 5H2V4h4v1zM2 8h7V7H2v1zm0 2h7V9H2v1zm0 2h7v-1H2v1zm10-7.5V14c0 .55-.45 1-1 1H1c-.55 0-1-.45-1-1V2c0-.55.45-1 1-1h7.5L12 4.5zM11 5L8 2H1v12h10V5z",
    }],
  },
  download: {
    width: 16,
    viewBox: "0 0 16 16",
    paths: [
      { d: "M.5 9.9a.5.5 0 0 1 .5.5v2.5a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1v-2.5a.5.5 0 0 1 1 0v2.5a2 2 0 0 1-2 2H2a2 2 0 0 1-2-2v-2.5a.5.5 0 0 1 .5-.5z" },
      { d: "M7.646 11.854a.5.5 0 0 0 .708 0l3-3a.5.5 0 0 0-.708-.708L8.5 10.293V1.5a.5.5 0 0 0-1 0v8.793L5.354 8.146a.5.5 0 1 0-.708.708l3 3z" },
    ],
  },
  move: {
    width: 16,
    viewBox: "0 0 16 16",
    paths: [{
      fillRule: "evenodd",
      d: "M1.5 1.5A.5.5 0 0 0 1 2v4.8a2.5 2.5 0 0 0 2.5 2.5h9.793l-3.347 3.346a.5.5 0 0 0 .708.708l4.2-4.2a.5.5 0 0 0 0-.708l-4-4a.5.5 0 0 0-.708.708L13.293 8.3H3.5A1.5 1.5 0 0 1 2 6.8V2a.5.5 0 0 0-.5-.5z",
    }],
  },
  rename: {
    width: 16,
    viewBox: "0 0 16 16",
    paths: [{
      d: "M11.013 1.427a1.75 1.75 0 0 1 2.475 0l1.085 1.085a1.75 1.75 0 0 1 0 2.475L5.25 14.31l-4.43.87.87-4.43 9.323-9.323zm1.414 1.06a.75.75 0 0 0-1.06 0L10.25 3.604l2.146 2.146 1.117-1.117a.75.75 0 0 0 0-1.06l-1.086-1.086zM11.336 6.81 9.19 4.664l-6.57 6.57-.45 2.295 2.295-.45 6.87-6.269z",
    }],
  },
  delete: {
    width: 16,
    viewBox: "0 0 16 16",
    paths: [
      { d: "M6.854 7.146a.5.5 0 1 0-.708.708L7.293 9l-1.147 1.146a.5.5 0 0 0 .708.708L8 9.707l1.146 1.147a.5.5 0 0 0 .708-.708L8.707 9l1.147-1.146a.5.5 0 0 0-.708-.708L8 8.293 6.854 7.146z" },
      { d: "M14 14V4.5L9.5 0H4a2 2 0 0 0-2 2v12a2 2 0 0 0 2 2h8a2 2 0 0 0 2-2zM9.5 3A1.5 1.5 0 0 0 11 4.5h2V14a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1V2a1 1 0 0 1 1-1h5.5v2z" },
    ],
  },
});

/**
 * @param {string} tagName
 * @param {{
 *   className?: string,
 *   text?: unknown,
 *   attributes?: Record<string, string | number | boolean | null | undefined>,
 * }} [options]
 * @returns {HTMLElement}
 */
export function createElement(tagName, options = {}) {
  const element = document.createElement(tagName);
  if (options.className) element.className = options.className;
  if (options.text !== undefined) element.textContent = String(options.text);
  for (const [name, value] of Object.entries(options.attributes || {})) {
    if (value !== undefined && value !== null && value !== false) {
      element.setAttribute(name, value === true ? "" : String(value));
    }
  }
  return element;
}

/**
 * @param {string} name
 * @param {string} [accessibleName]
 * @returns {SVGSVGElement}
 */
export function createIcon(name, accessibleName = "") {
  const definition = ICONS[name] || ICONS.file;
  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("width", String(definition.width));
  svg.setAttribute("height", "16");
  svg.setAttribute("viewBox", definition.viewBox);
  if (accessibleName) {
    svg.setAttribute("role", "img");
    svg.setAttribute("aria-label", accessibleName);
  } else {
    svg.setAttribute("aria-hidden", "true");
  }
  for (const item of definition.paths) {
    const path = document.createElementNS(SVG_NS, "path");
    path.setAttribute("d", item.d);
    if (item.fillRule) path.setAttribute("fill-rule", item.fillRule);
    svg.append(path);
  }
  return svg;
}

/** @param {unknown} error */
export function errorMessage(error) {
  return error instanceof Error && error.message ? error.message : "Unknown error";
}

/** @param {number} size @returns {[number, string]} */
export function formatFileSize(size) {
  if (!Number.isFinite(size) || size <= 0) return [0, "B"];
  const units = ["B", "KB", "MB", "GB", "TB", "PB", "EB"];
  const index = Math.min(
    Math.floor(Math.log(size) / Math.log(1024)),
    units.length - 1,
  );
  const raw = size / Math.pow(1024, index);
  const value = index > 0 && raw < 999.95
    ? Math.round(raw * 10) / 10
    : Math.round(raw);
  return [value, units[index]];
}
