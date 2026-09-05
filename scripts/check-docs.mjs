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
const invocation = parseInvocation(process.argv.slice(2));
const projectRoot = realpathSync(invocation.projectRoot);
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

const currentNodeVersion = "26.7.0";
const nodeVersionSource = readFileSync(
  resolve(projectRoot, ".node-version"),
  "utf8",
);
const packageSource = readFileSync(resolve(projectRoot, "package.json"), "utf8");
const packageLockSource = readFileSync(
  resolve(projectRoot, "package-lock.json"),
  "utf8",
);
const workflowDirectory = resolve(projectRoot, ".github", "workflows");
const workflowSources = invocation.checkWorkflowContracts
  ? new Map(
      readdirSync(workflowDirectory)
        .filter(name => name.endsWith(".yml") || name.endsWith(".yaml"))
        .map(name => [name, readFileSync(resolve(workflowDirectory, name), "utf8")]),
    )
  : new Map();
failures.push(
  ...currentNodeContractFailures(
    nodeVersionSource,
    packageSource,
    packageLockSource,
    workflowSources,
  ),
);
if (
  currentNodeContractFailures(
    nodeVersionSource,
    packageSource.replace(
      `"node": ">=${currentNodeVersion} <27"`,
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
if (
  currentNodeContractFailures(
    "18.20.8\n",
    packageSource,
    packageLockSource,
    workflowSources,
  ).length === 0
) {
  failures.push(
    "scripts/check-docs.mjs: old .node-version mutation fixture was accepted",
  );
}
if (
  currentNodeContractFailures(
    `${currentNodeVersion}\n\n`,
    packageSource,
    packageLockSource,
    workflowSources,
  ).length === 0
) {
  failures.push(
    "scripts/check-docs.mjs: malformed .node-version mutation fixture was accepted",
  );
}
if (invocation.checkWorkflowContracts) {
  const mutatedWorkflows = new Map(workflowSources);
  mutatedWorkflows.set(
    "read-only-ci.yml",
    workflowSources
      .get("read-only-ci.yml")
      .replace("node-version: ${{ env.NODE_VERSION }}", 'node-version: "18.20.8"'),
  );
  if (
    currentNodeContractFailures(
      nodeVersionSource,
      packageSource,
      packageLockSource,
      mutatedWorkflows,
    ).length === 0
  ) {
    failures.push(
      "scripts/check-docs.mjs: old Node workflow mutation fixture was accepted",
    );
  }
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

function parseInvocation(args) {
  if (args.length === 0) {
    return {
      projectRoot: defaultProjectRoot,
      checkWorkflowContracts: true,
    };
  }
  if (args.length === 2 && args[0] === "--artifact-root") {
    return {
      projectRoot: resolve(args[1]),
      checkWorkflowContracts: false,
    };
  }
  throw new Error("Usage: check-docs.mjs [--artifact-root <directory>]");
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

function currentNodeContractFailures(
  nodeVersionFileSource,
  manifestSource,
  lockSource,
  checkedWorkflows,
) {
  const nodeFailures = [];
  let manifest;
  let lock;
  if (nodeVersionFileSource !== `${currentNodeVersion}\n`) {
    nodeFailures.push(
      `.node-version: must be exactly ${currentNodeVersion} followed by one LF`,
    );
  }
  try {
    manifest = JSON.parse(manifestSource);
    lock = JSON.parse(lockSource);
  } catch {
    return ["package.json and package-lock.json must be valid JSON"];
  }
  const currentNodeRange = `>=${currentNodeVersion} <27`;
  if (manifest.engines?.node !== currentNodeRange) {
    nodeFailures.push(
      `package.json: engines.node must be exactly ${currentNodeRange}`,
    );
  }
  if (lock.packages?.[""]?.engines?.node !== currentNodeRange) {
    nodeFailures.push(
      `package-lock.json: root engines.node must be exactly ${currentNodeRange}`,
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
