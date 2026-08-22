import {
  chmodSync,
  closeSync,
  constants,
  mkdtempSync,
  mkdirSync,
  openSync,
  readFileSync,
  realpathSync,
  renameSync,
  rmSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import {
  dirname,
  isAbsolute,
  join,
  resolve,
  win32,
} from "node:path";
import { pathToFileURL } from "node:url";

const MAX_URL_DECODE_PASSES = 16;

const args = process.argv.slice(2);
if (args.length === 1 && args[0] === "--self-test") {
  runSelfTest();
  process.exit(0);
}

const [sbomPath, sourcePath, version, sourceRevision, buildRoot, ...extra] =
  args;
if (
  !sbomPath ||
  !sourcePath ||
  !version ||
  !sourceRevision ||
  !buildRoot ||
  extra.length > 0
) {
  throw new Error(
    "Usage: normalize-sbom.mjs <SBOM> <source-path> <version> " +
      "<source-revision> <build-root>",
  );
}

const context = createContext(sourcePath, version, sourceRevision, buildRoot);
const bom = JSON.parse(readFileSync(sbomPath, "utf8"));
const normalized = normalizeBom(bom, context);
writeJsonAtomically(sbomPath, normalized);

function createContext(sourcePath, version, sourceRevision, buildRoot) {
  if (!/^(?:[0-9a-f]{40}|[0-9a-f]{64})$/u.test(sourceRevision)) {
    throw new Error(
      "source revision must be a full lowercase hexadecimal object ID",
    );
  }
  if (!/^[0-9A-Za-z][0-9A-Za-z.+-]*$/u.test(version)) {
    throw new Error("version is not safe for a Cargo package URL");
  }

  const resolvedSource = resolve(sourcePath);
  const physicalSource = realpathSync(resolvedSource);
  const resolvedBuildRoot = realpathSync(resolve(buildRoot));
  const sourceUrls = new Set([
    pathToFileURL(resolvedSource).href,
    pathToFileURL(physicalSource).href,
  ]);
  const localPackageIds = new Set(
    [...sourceUrls].map(url => `path+${url}#dufs@${version}`),
  );
  const stablePackageId =
    `pkg:cargo/dufs@${version}?source_revision=${sourceRevision}`;

  return {
    localPackageIds,
    physicalSource,
    resolvedBuildRoot,
    resolvedSource,
    sourceRevision,
    sourceUrls,
    stablePackageId,
    version,
  };
}

function normalizeBom(bom, context) {
  if (!bom || typeof bom !== "object" || Array.isArray(bom)) {
    throw new Error("CycloneDX document must be a JSON object");
  }
  const counters = {
    localPurls: 0,
    localReferences: 0,
  };
  const normalized = normalizeNode(bom, "", context, counters);
  const root = normalized?.metadata?.component;
  if (
    !root ||
    root.name !== "dufs" ||
    root.version !== context.version ||
    root["bom-ref"] !== context.stablePackageId ||
    root.purl !== context.stablePackageId
  ) {
    throw new Error("normalized SBOM does not contain the expected Dufs root");
  }
  const rootDependency = normalized.dependencies?.filter(
    dependency => dependency?.ref === context.stablePackageId,
  );
  if (!rootDependency || rootDependency.length !== 1) {
    throw new Error(
      "normalized SBOM must contain exactly one Dufs dependency root",
    );
  }
  if (counters.localReferences < 2 || counters.localPurls < 1) {
    throw new Error(
      "expected local Dufs references and package URLs were not normalized",
    );
  }
  assertNoLocalLeaks(normalized, context);
  return normalized;
}

function normalizeNode(value, key, context, counters) {
  if (Array.isArray(value)) {
    return value.map(item => normalizeNode(item, key, context, counters));
  }
  if (value && typeof value === "object") {
    return Object.fromEntries(
      Object.entries(value).map(([childKey, childValue]) => [
        childKey,
        normalizeNode(childValue, childKey, context, counters),
      ]),
    );
  }
  if (typeof value !== "string") return value;

  for (const localPackageId of context.localPackageIds) {
    if (
      value === localPackageId ||
      value.startsWith(`${localPackageId} `)
    ) {
      counters.localReferences += 1;
      return `${context.stablePackageId}${value.slice(localPackageId.length)}`;
    }
  }
  if (key === "purl" && isLocalDufsPurl(value, context)) {
    counters.localPurls += 1;
    const fragmentIndex = value.indexOf("#");
    const fragment = fragmentIndex === -1 ? "" : value.slice(fragmentIndex);
    return `${context.stablePackageId}${fragment}`;
  }
  return value;
}

function isLocalDufsPurl(value, context) {
  const fragmentIndex = value.indexOf("#");
  const withoutFragment =
    fragmentIndex === -1 ? value : value.slice(0, fragmentIndex);
  const prefix = `pkg:cargo/dufs@${context.version}?`;
  if (!withoutFragment.startsWith(prefix)) return false;
  const query = withoutFragment.slice(prefix.length);
  const parameters = new URLSearchParams(query);
  const downloadUrl = parameters.get("download_url");
  return downloadUrl !== null && /^file:/iu.test(downloadUrl);
}

function assertNoLocalLeaks(value, context, location = "$") {
  if (Array.isArray(value)) {
    value.forEach((item, index) => {
      assertNoLocalLeaks(item, context, `${location}[${index}]`);
    });
    return;
  }
  if (value && typeof value === "object") {
    Object.entries(value).forEach(([key, child]) => {
      assertNoLocalLeaks(child, context, `${location}.${key}`);
    });
    return;
  }
  if (typeof value !== "string") return;

  const candidates = decodedCandidates(value);
  for (const candidate of candidates) {
    const lower = candidate.toLowerCase();
    if (
      lower.includes("path+file:") ||
      lower.includes("download_url=file:") ||
      lower.includes("file://") ||
      lower.startsWith("file:") ||
      candidate.includes(context.resolvedSource) ||
      candidate.includes(context.physicalSource) ||
      candidate.includes(context.resolvedBuildRoot) ||
      [...context.sourceUrls].some(url => candidate.includes(url)) ||
      isAbsolute(candidate) ||
      win32.isAbsolute(candidate) ||
      /(?:^|[\s"'=(])\/(?:home|mnt|root|tmp|var\/tmp)\//u.test(candidate)
    ) {
      throw new Error(`normalized SBOM leaks a local path at ${location}`);
    }
  }
}

function decodedCandidates(value) {
  const candidates = new Set([value]);
  let candidate = value;
  for (let pass = 0; pass < MAX_URL_DECODE_PASSES; pass++) {
    const decoded = decodeCandidateOnce(candidate);
    if (decoded === candidate) break;
    candidates.add(decoded);
    candidate = decoded;
  }
  if (decodeCandidateOnce(candidate) !== candidate) {
    throw new Error(
      "normalized SBOM contains excessively nested URL encoding",
    );
  }
  return candidates;
}

function decodeCandidateOnce(value) {
  try {
    return decodeURIComponent(value);
  } catch {
    // A malformed escape elsewhere in the field must not prevent valid
    // ASCII path delimiters from being exposed. Decode one percent layer
    // byte-by-byte and leave non-ASCII bytes for a later whole-string pass.
    return value.replace(/%([0-9a-f]{2})/giu, (encoded, hex) => {
      const byte = Number.parseInt(hex, 16);
      return byte <= 0x7f ? String.fromCharCode(byte) : encoded;
    });
  }
}

function writeJsonAtomically(path, value) {
  const output = `${JSON.stringify(value, null, 2)}\n`;
  const temporaryPath =
    join(dirname(resolve(path)), `.dufs-sbom-${process.pid}.tmp`);
  let descriptor;
  try {
    descriptor = openSync(
      temporaryPath,
      constants.O_CREAT | constants.O_EXCL | constants.O_WRONLY,
      0o600,
    );
    writeFileSync(descriptor, output, "utf8");
    closeSync(descriptor);
    descriptor = undefined;
    chmodSync(temporaryPath, 0o644);
    renameSync(temporaryPath, path);
  } finally {
    if (descriptor !== undefined) closeSync(descriptor);
    try {
      unlinkSync(temporaryPath);
    } catch (error) {
      if (error?.code !== "ENOENT") throw error;
    }
  }
}

function runSelfTest() {
  const testRoot = mkdtempSync(join(tmpdir(), "dufs-sbom-self-test-"));
  try {
    const sourcePath = join(testRoot, "build source");
    const version = "1.2.3";
    const sourceRevision = "0123456789abcdef0123456789abcdef01234567";
    mkdirSync(sourcePath, { recursive: true });
    const sha256Revision = "0123456789abcdef".repeat(4);
    if (
      createContext(sourcePath, version, sha256Revision, testRoot)
        .sourceRevision !== sha256Revision
    ) {
      throw new Error("SBOM self-test rejected a 64-character object ID");
    }
    for (const invalidLength of [39, 41, 63, 65]) {
      let rejectedRevision = false;
      try {
        createContext(
          sourcePath,
          version,
          "a".repeat(invalidLength),
          testRoot,
        );
      } catch {
        rejectedRevision = true;
      }
      if (!rejectedRevision) {
        throw new Error(
          `SBOM self-test accepted a ${invalidLength}-character object ID`,
        );
      }
    }
    const context = createContext(
      sourcePath,
      version,
      sourceRevision,
      testRoot,
    );
    const [localId] = context.localPackageIds;
    const stableId = context.stablePackageId;
    const fixture = {
      bomFormat: "CycloneDX",
      specVersion: "1.5",
      metadata: {
        component: {
          "bom-ref": localId,
          name: "dufs",
          version,
          purl: `pkg:cargo/dufs@${version}?download_url=file://.`,
          components: [
            {
              "bom-ref": `${localId} bin-target-0`,
              name: "dufs",
              purl:
                `pkg:cargo/dufs@${version}?download_url=file://.` +
                "#src/lib.rs",
              version,
            },
          ],
        },
      },
      components: [
        {
          "bom-ref": "registry+https://example.invalid#index@1.0.0",
          name: "index",
          purl: "pkg:cargo/index@1.0.0",
          version: "1.0.0",
        },
      ],
      dependencies: [
        {
          ref: localId,
          dependsOn: ["registry+https://example.invalid#index@1.0.0"],
        },
      ],
    };
    const normalized = normalizeBom(fixture, context);
    if (
      normalized.metadata.component.purl !== stableId ||
      normalized.metadata.component.components[0].purl !==
        `${stableId}#src/lib.rs`
    ) {
      throw new Error("SBOM self-test did not normalize nested package URLs");
    }

    const leaked = structuredClone(normalized);
    leaked.metadata.component.description = `${testRoot}/secret`;
    let rejectedLeak = false;
    try {
      assertNoLocalLeaks(leaked, context);
    } catch {
      rejectedLeak = true;
    }
    if (!rejectedLeak) {
      throw new Error("SBOM self-test accepted a local build path");
    }

    const encodedLeaks = [
      "path%252Bfile%253A%252F%252F%252Ftmp%252Fsecret",
      "download_url=file%25253A%25252F%25252F%25252Froot%25252Fsecret",
      "C%253A%255CUsers%255Crelease%255Csecret",
      "%25252Fmnt%25252Frelease%25252Fsecret",
      "%252Ftmp%252Fsecret%ZZ",
    ];
    let excessivelyEncodedLeak = "/tmp/release-secret";
    for (let pass = 0; pass <= MAX_URL_DECODE_PASSES; pass++) {
      excessivelyEncodedLeak = encodeURIComponent(excessivelyEncodedLeak);
    }
    encodedLeaks.push(excessivelyEncodedLeak);
    for (const encodedLeak of encodedLeaks) {
      const encoded = structuredClone(normalized);
      encoded.metadata.component.description = encodedLeak;
      let rejectedEncodedLeak = false;
      try {
        assertNoLocalLeaks(encoded, context);
      } catch {
        rejectedEncodedLeak = true;
      }
      if (!rejectedEncodedLeak) {
        throw new Error(
          `SBOM self-test accepted encoded local path: ${encodedLeak}`,
        );
      }
    }

    const normalizedPath = join(testRoot, "normalized.cdx.json");
    writeJsonAtomically(normalizedPath, normalized);
    const roundTrip = JSON.parse(readFileSync(normalizedPath, "utf8"));
    if (roundTrip.metadata.component["bom-ref"] !== stableId) {
      throw new Error("SBOM self-test atomic write changed the document");
    }
  } finally {
    rmSync(testRoot, { force: true, recursive: true });
  }
  process.stdout.write("recursive SBOM normalization self-test passed\n");
}
