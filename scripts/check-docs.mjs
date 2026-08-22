import {
  existsSync,
  lstatSync,
  readdirSync,
  readFileSync,
  realpathSync,
} from "node:fs";
import {
  dirname,
  extname,
  isAbsolute,
  join,
  relative,
  resolve,
  sep,
} from "node:path";
import { fileURLToPath } from "node:url";

const defaultProjectRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const projectRoot = realpathSync(parseProjectRoot(process.argv.slice(2)));
const ignoredGeneratedDirectories = new Set([
  ".dufs-data",
  ".git",
  "dist",
  "node_modules",
  "playwright-report",
  "target",
  "test-results",
]);
const failures = [];
const markdownFiles = walk(projectRoot, failures)
  .filter(path => extname(path) === ".md")
  .sort();
const headingsByFile = new Map();

const referenceFixture = "[operations][guide]\n\n[guide]: docs/missing.md\n";
if (!markdownTargets(referenceFixture).includes("docs/missing.md")) {
  failures.push(
    "scripts/check-docs.mjs: reference-style link regression fixture was not detected",
  );
}
const fencedFixture = "```md\n[ignored](docs/missing.md)\n```\n";
if (markdownTargets(maskFencedCode(fencedFixture)).length !== 0) {
  failures.push(
    "scripts/check-docs.mjs: fenced-code link regression fixture was not ignored",
  );
}

for (const path of markdownFiles) {
  const source = readFileSync(path, "utf8");
  const name = relative(projectRoot, path);
  checkTextFormat(name, source);
  headingsByFile.set(path, markdownHeadings(maskFencedCode(source)));
}

for (const path of markdownFiles) {
  const source = readFileSync(path, "utf8");
  const markdownSource = maskFencedCode(source);
  const name = relative(projectRoot, path);
  for (const rawTarget of markdownTargets(markdownSource)) {
    let target = rawTarget.trim();
    if (target.startsWith("<") && target.endsWith(">")) {
      target = target.slice(1, -1);
    }
    if (
      !target ||
      /^(?:https?:|mailto:|data:)/iu.test(target)
    ) {
      continue;
    }
    const [rawPath, rawFragment = ""] = target.split("#", 2);
    let decodedPath;
    let decodedFragment;
    try {
      decodedPath = decodeURIComponent(rawPath);
      decodedFragment = decodeURIComponent(rawFragment);
    } catch {
      failures.push(`${name}: link contains invalid percent encoding: ${target}`);
      continue;
    }
    const destination = decodedPath
      ? resolve(dirname(path), decodedPath)
      : path;
    const destinationFromRoot = relative(projectRoot, destination);
    if (
      destinationFromRoot === ".." ||
      destinationFromRoot.startsWith(`..${sep}`) ||
      isAbsolute(destinationFromRoot)
    ) {
      failures.push(`${name}: local link escapes the documentation root: ${target}`);
      continue;
    }
    if (!existsSync(destination)) {
      failures.push(`${name}: local link target does not exist: ${target}`);
      continue;
    }
    let realDestination;
    try {
      realDestination = realpathSync(destination);
    } catch {
      failures.push(`${name}: local link target cannot be resolved: ${target}`);
      continue;
    }
    const realDestinationFromRoot = relative(projectRoot, realDestination);
    if (
      realDestinationFromRoot === ".." ||
      realDestinationFromRoot.startsWith(`..${sep}`) ||
      isAbsolute(realDestinationFromRoot)
    ) {
      failures.push(
        `${name}: resolved local link escapes the documentation root: ${target}`,
      );
      continue;
    }
    if (
      decodedFragment &&
      extname(realDestination).toLowerCase() === ".md" &&
      !headingsByFile.get(realDestination)?.has(decodedFragment.toLowerCase())
    ) {
      failures.push(`${name}: Markdown heading does not exist: ${target}`);
    }
  }
}

function parseProjectRoot(args) {
  if (args.length === 0) return defaultProjectRoot;
  if (args.length === 2 && args[0] === "--root") {
    return resolve(args[1]);
  }
  throw new Error("Usage: check-docs.mjs [--root <directory>]");
}

if (failures.length > 0) {
  process.stderr.write(`${failures.join("\n")}\n`);
  process.exit(1);
}
process.stdout.write(
  `Markdown formatting and local-link checks passed for ${markdownFiles.length} files\n`,
);

function walk(root, walkFailures) {
  const output = [];
  for (const entry of readdirSync(root, { withFileTypes: true })) {
    if (ignoredGeneratedDirectories.has(entry.name)) continue;
    const path = join(root, entry.name);
    const metadata = lstatSync(path);
    if (metadata.isSymbolicLink()) {
      walkFailures.push(
        `${relative(projectRoot, path)}: symbolic links are not allowed in the checked tree`,
      );
    } else if (metadata.isDirectory()) {
      output.push(...walk(path, walkFailures));
    } else if (metadata.isFile()) {
      output.push(path);
    }
  }
  return output;
}

function checkTextFormat(name, source) {
  if (source.startsWith("\uFEFF")) failures.push(`${name}: UTF-8 BOM is not allowed`);
  if (source.includes("\r")) failures.push(`${name}: use LF line endings`);
  if (!source.endsWith("\n")) failures.push(`${name}: file must end with a newline`);
  source.split("\n").forEach((line, index) => {
    if (/[ \t]+$/u.test(line)) {
      failures.push(`${name}:${index + 1}: trailing whitespace`);
    }
  });
}

function markdownTargets(source) {
  const targets = [];
  for (const match of source.matchAll(/!?\[[^\]\n]*\]\(([^)\n]+)\)/gu)) {
    targets.push(match[1]);
  }
  for (const match of source.matchAll(
    /^ {0,3}\[([^\]\n]+)\]:[ \t]*(?:<([^>\n]+)>|(\S+))/gmu,
  )) {
    if (!match[1].startsWith("^")) {
      targets.push(match[2] || match[3]);
    }
  }
  return targets;
}

function markdownHeadings(source) {
  const slugs = new Set();
  const counts = new Map();
  const lines = source.split("\n");
  for (let index = 0; index < lines.length; index += 1) {
    const atx = /^(?: {0,3})#{1,6}\s+(.+?)\s*#*\s*$/u.exec(lines[index]);
    const setext =
      index + 1 < lines.length &&
      /^(?: {0,3})(?:=+|-+)\s*$/u.test(lines[index + 1]) &&
      lines[index].trim()
        ? lines[index].trim()
        : null;
    const heading = atx?.[1] || setext;
    if (!heading) continue;
    const base = heading
      .toLowerCase()
      .replace(/<[^>]*>/gu, "")
      .replace(/[^\p{Letter}\p{Number}\s_-]/gu, "")
      .trim()
      .replace(/\s+/gu, "-");
    const count = counts.get(base) || 0;
    counts.set(base, count + 1);
    slugs.add(count === 0 ? base : `${base}-${count}`);
  }
  return slugs;
}

function maskFencedCode(source) {
  let fence = null;
  return source
    .split("\n")
    .map(line => {
      if (fence === null) {
        const opening = /^ {0,3}(`{3,}|~{3,})/u.exec(line);
        if (!opening) return line;
        fence = { marker: opening[1][0], length: opening[1].length };
        return "";
      }
      const trimmed = line.replace(/^ {0,3}/u, "");
      const closing = new RegExp(
        `^${fence.marker}{${fence.length},}[ \\t]*$`,
        "u",
      );
      if (closing.test(trimmed)) fence = null;
      return "";
    })
    .join("\n");
}
