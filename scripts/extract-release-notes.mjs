import { readFileSync } from "node:fs";

const semverPattern =
  /^(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$/u;
const versionHeadingPattern =
  /^## \[([^\]\n]+)\](?: - [0-9]{4}-[0-9]{2}-[0-9]{2})?[\t ]*$/u;

function normalizeMarkdown(source) {
  if (source.includes("\0")) {
    throw new Error("CHANGELOG contains a NUL byte");
  }
  const normalized = source.replaceAll("\r\n", "\n");
  if (normalized.includes("\r")) {
    throw new Error("CHANGELOG contains an unsupported bare carriage return");
  }
  return normalized;
}

function extractReleaseNotes(source, version) {
  if (!semverPattern.test(version)) {
    throw new Error(`invalid release version: ${version}`);
  }

  const lines = normalizeMarkdown(source).split("\n");
  const starts = [];
  for (const [index, line] of lines.entries()) {
    const match = versionHeadingPattern.exec(line);
    if (match?.[1] === version) {
      starts.push(index);
    }
  }
  if (starts.length !== 1) {
    throw new Error(
      `expected exactly one CHANGELOG section for ${version}; found ${starts.length}`,
    );
  }

  let start = starts[0] + 1;
  let end = lines.length;
  for (let index = start; index < lines.length; index += 1) {
    if (lines[index].startsWith("## ")) {
      end = index;
      break;
    }
  }

  while (start < end && lines[start] === "") {
    start += 1;
  }
  while (end > start && lines[end - 1] === "") {
    end -= 1;
  }
  const notes = lines.slice(start, end).join("\n");
  if (notes.trim() === "") {
    throw new Error(`CHANGELOG section for ${version} is empty`);
  }
  return `${notes}\n`;
}

function expectFailure(source, version, expectedMessage) {
  try {
    extractReleaseNotes(source, version);
  } catch (error) {
    if (error instanceof Error && error.message.includes(expectedMessage)) {
      return;
    }
    throw error;
  }
  throw new Error(`fixture unexpectedly accepted: ${expectedMessage}`);
}

function selfTest() {
  const fixture = [
    "# Changelog",
    "",
    "## [1.2.30] - 2026-01-02",
    "",
    "wrong prefix",
    "",
    "## [1.2.3] - 2026-01-01",
    "",
    "### Fixed",
    "",
    "- exact section",
    "",
    "## [1.2.2]",
    "",
    "older section",
    "",
  ].join("\r\n");
  const expected = "### Fixed\n\n- exact section\n";
  if (extractReleaseNotes(fixture, "1.2.3") !== expected) {
    throw new Error("exact-section fixture produced unexpected notes");
  }

  expectFailure(fixture, "1.2.4", "found 0");
  expectFailure(
    `${fixture}\n## [1.2.3]\n\nduplicate\n`,
    "1.2.3",
    "found 2",
  );
  expectFailure("## [1.2.3]\n\n## [1.2.2]\nbody\n", "1.2.3", "empty");
  expectFailure(fixture, "01.2.3", "invalid release version");
  expectFailure("## [1.2.3]\rbody\n", "1.2.3", "bare carriage return");
  expectFailure("## [1.2.3]\n\0\n", "1.2.3", "NUL byte");
  process.stdout.write("release-note extractor self-test passed\n");
}

function main(args) {
  if (args.length === 1 && args[0] === "--self-test") {
    selfTest();
    return;
  }
  if (args.length !== 2) {
    throw new Error(
      "usage: node scripts/extract-release-notes.mjs CHANGELOG.md VERSION",
    );
  }
  const [path, version] = args;
  process.stdout.write(extractReleaseNotes(readFileSync(path, "utf8"), version));
}

try {
  main(process.argv.slice(2));
} catch (error) {
  const message = error instanceof Error ? error.message : String(error);
  process.stderr.write(`extract-release-notes: ${message}\n`);
  process.exitCode = 1;
}
