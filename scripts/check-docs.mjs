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

const v0497ReleaseFacts = Object.freeze({
  commit: "5b098e2a8f05557b72efdf7929f4ccef3a3af837",
  binaryName: "dufs-0.49.7-x86_64-unknown-linux-gnu",
  binaryBytes: 6025624,
  binarySha256:
    "4dd74e3164fbffcb3765c2c33c518ab9c24e7571bd23f5206fc6ce3802ddd66b",
  checksumBytes: 103,
  checksumSha256:
    "a282bbad570d55eabef56d41a0501b6182acac2d802ff569c90f4e014cd120b4",
});
const releaseFactFixture = [
  "## [0.49.7] - 2026-08-26",
  "",
  `> **发布状态：** 附注标签 \`v0.49.7\` 未签名，当前实际解引用到提交 \`${v0497ReleaseFacts.commit}\`。` +
    ` \`${v0497ReleaseFacts.binaryName}\` 二进制（\`${v0497ReleaseFacts.binaryBytes}\` 字节，SHA-256 \`${v0497ReleaseFacts.binarySha256}\`）和` +
    ` \`${v0497ReleaseFacts.binaryName}.sha256\` 校验和文件（\`${v0497ReleaseFacts.checksumBytes}\` 字节，文件自身 SHA-256 \`${v0497ReleaseFacts.checksumSha256}\`）。`,
  "",
  "## [0.49.6] - 2026-08-25",
].join("\n");
if (v0497ReleaseFactFailures(releaseFactFixture).length !== 0) {
  failures.push(
    "scripts/check-docs.mjs: valid v0.49.7 release-fact fixture was rejected",
  );
}
if (
  v0497ReleaseFactFailures(
    releaseFactFixture.replace(v0497ReleaseFacts.commit, "0".repeat(40)),
  ).length === 0
) {
  failures.push(
    "scripts/check-docs.mjs: incorrect v0.49.7 source commit fixture was accepted",
  );
}

const currentNodeVersion = "24.8.0";
const packageSource = readFileSync(resolve(projectRoot, "package.json"), "utf8");
const packageLockSource = readFileSync(
  resolve(projectRoot, "package-lock.json"),
  "utf8",
);
const workflowDirectory = resolve(projectRoot, ".github", "workflows");
const workflowSources = new Map(
  readdirSync(workflowDirectory)
    .filter(name => name.endsWith(".yml") || name.endsWith(".yaml"))
    .map(name => [name, readFileSync(resolve(workflowDirectory, name), "utf8")]),
);
failures.push(
  ...currentNodeContractFailures(
    packageSource,
    packageLockSource,
    workflowSources,
  ),
);
if (
  currentNodeContractFailures(
    packageSource.replace(
      `"node": "${currentNodeVersion}"`,
      '"node": ">=18"',
    ),
    packageLockSource,
    workflowSources,
  ).length === 0
) {
  failures.push(
    "scripts/check-docs.mjs: old Node engine mutation fixture was accepted",
  );
}
const mutatedWorkflows = new Map(workflowSources);
mutatedWorkflows.set(
  "read-only-ci.yml",
  workflowSources
    .get("read-only-ci.yml")
    .replace("node-version: ${{ env.NODE_VERSION }}", 'node-version: "18.20.8"'),
);
if (
  currentNodeContractFailures(
    packageSource,
    packageLockSource,
    mutatedWorkflows,
  ).length === 0
) {
  failures.push(
    "scripts/check-docs.mjs: old Node workflow mutation fixture was accepted",
  );
}

for (const path of markdownFiles) {
  const source = readFileSync(path, "utf8");
  const name = relative(projectRoot, path);
  checkTextFormat(name, source);
  if (name === "CHANGELOG.md") {
    failures.push(...v0497ReleaseFactFailures(source));
  }
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

function v0497ReleaseFactFailures(source) {
  const releaseFailures = [];
  const headings = [...source.matchAll(/^## \[0\.49\.7\][^\n]*$/gmu)];
  if (headings.length !== 1) {
    return [
      `CHANGELOG.md: expected exactly one v0.49.7 section; found ${headings.length}`,
    ];
  }
  const sectionStart = headings[0].index;
  const nextSection = source.indexOf("\n## [", sectionStart + headings[0][0].length);
  const section = source.slice(
    sectionStart,
    nextSection === -1 ? source.length : nextSection,
  );
  const statusLines = section
    .split("\n")
    .filter(line => line.startsWith("> **发布状态：**"));
  if (statusLines.length !== 1) {
    return [
      `CHANGELOG.md: expected exactly one v0.49.7 release-status line; found ${statusLines.length}`,
    ];
  }
  const status = statusLines[0];
  const requiredFacts = [
    `附注标签 \`v0.49.7\` 未签名，当前实际解引用到提交 \`${v0497ReleaseFacts.commit}\``,
    `\`${v0497ReleaseFacts.binaryName}\` 二进制（\`${v0497ReleaseFacts.binaryBytes}\` 字节，SHA-256 \`${v0497ReleaseFacts.binarySha256}\`）`,
    `\`${v0497ReleaseFacts.binaryName}.sha256\` 校验和文件（\`${v0497ReleaseFacts.checksumBytes}\` 字节，文件自身 SHA-256 \`${v0497ReleaseFacts.checksumSha256}\`）`,
  ];
  for (const fact of requiredFacts) {
    if (!status.includes(fact)) {
      releaseFailures.push(
        `CHANGELOG.md: v0.49.7 release status is missing the pinned fact: ${fact}`,
      );
    }
  }
  if (status.includes("同一 tag/SHA")) {
    releaseFailures.push(
      "CHANGELOG.md: v0.49.7 historical workflow runs must not be described as the current tag/SHA identity",
    );
  }
  return releaseFailures;
}

function currentNodeContractFailures(
  manifestSource,
  lockSource,
  checkedWorkflows,
) {
  const nodeFailures = [];
  let manifest;
  let lock;
  try {
    manifest = JSON.parse(manifestSource);
    lock = JSON.parse(lockSource);
  } catch {
    return ["package.json and package-lock.json must be valid JSON"];
  }
  if (manifest.engines?.node !== currentNodeVersion) {
    nodeFailures.push(
      `package.json: engines.node must be exactly ${currentNodeVersion}`,
    );
  }
  if (lock.packages?.[""]?.engines?.node !== currentNodeVersion) {
    nodeFailures.push(
      `package-lock.json: root engines.node must be exactly ${currentNodeVersion}`,
    );
  }

  for (const [name, source] of checkedWorkflows) {
    const versions = [
      ...source.matchAll(/^\s+node-version:\s*(\S(?:.*\S)?)\s*$/gmu),
    ];
    if (versions.length === 0) continue;
    const declarations = [
      ...source.matchAll(
        new RegExp(`^  NODE_VERSION: "${currentNodeVersion.replaceAll(".", "\\.")}"$`, "gmu"),
      ),
    ];
    if (declarations.length !== 1) {
      nodeFailures.push(
        `.github/workflows/${name}: must declare exactly one current NODE_VERSION`,
      );
    }
    for (const match of versions) {
      if (match[1] !== "${{ env.NODE_VERSION }}") {
        nodeFailures.push(
          `.github/workflows/${name}: node-version must use the current NODE_VERSION`,
        );
      }
    }
    if (/^  compatibility:/mu.test(source) || source.includes("18.20.8")) {
      nodeFailures.push(
        `.github/workflows/${name}: old Node compatibility jobs are forbidden`,
      );
    }
  }
  return nodeFailures;
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
