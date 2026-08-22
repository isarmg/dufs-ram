import {
  mkdtempSync,
  mkdirSync,
  readFileSync,
  realpathSync,
  rmSync,
} from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const arguments_ = process.argv.slice(2);
if (arguments_[0] === "--self-test") {
  if (arguments_.length !== 2) {
    throw new Error("Usage: seed-npm-cache.mjs --self-test <npm-cli>");
  }
  await runSelfTest(arguments_[1]);
  process.exit(0);
}
if (arguments_.length !== 4) {
  throw new Error(
    "Usage: seed-npm-cache.mjs " +
      "<package-lock.json> <source-cache> <destination-cache> <npm-cli>",
  );
}

const [lockPath, sourceCache, destinationCache, npmCli] = arguments_;
const cacache = loadCacache(npmCli);
const result = await seedCache(
  JSON.parse(readFileSync(lockPath, "utf8")),
  resolve(sourceCache),
  resolve(destinationCache),
  cacache,
);
process.stdout.write(
  `Seeded ${result.seeded} npm tarballs into the private cache; ` +
    `${result.missing} were unavailable in the source cache\n`,
);

function loadCacache(npmCliPath) {
  const physicalCli = realpathSync(resolve(npmCliPath));
  const requireFromNpm = createRequire(physicalCli);
  return requireFromNpm("cacache");
}

async function seedCache(lock, source, destination, cache) {
  if (
    !lock ||
    typeof lock !== "object" ||
    !lock.packages ||
    typeof lock.packages !== "object"
  ) {
    throw new Error("invalid package-lock document");
  }
  mkdirSync(destination, { recursive: true, mode: 0o700 });
  const sourceContentCache = join(source, "_cacache");
  const destinationContentCache = join(destination, "_cacache");
  const packages = Object.values(lock.packages)
    .filter(package_ => package_?.resolved)
    .sort((left, right) => left.resolved.localeCompare(right.resolved));
  let seeded = 0;
  let missing = 0;
  for (const package_ of packages) {
    const resolved = new URL(package_.resolved);
    if (
      resolved.protocol !== "https:" ||
      typeof package_.integrity !== "string" ||
      !package_.integrity.startsWith("sha512-")
    ) {
      throw new Error(
        `npm dependency is not bound to an HTTPS SHA-512 tarball: ` +
          `${package_.resolved}`,
      );
    }
    const key = `make-fetch-happen:request-cache:${package_.resolved}`;
    let record;
    try {
      record = await cache.get(sourceContentCache, key);
    } catch (error) {
      if (error?.code === "ENOENT") {
        missing++;
        continue;
      }
      throw error;
    }
    // Passing the lockfile integrity to put() re-hashes the bytes. A poisoned
    // cache entry therefore fails instead of being copied under a trusted key.
    await cache.put(destinationContentCache, key, record.data, {
      integrity: package_.integrity,
      metadata: {
        options: { compress: true },
        reqHeaders: {},
        resHeaders: {
          "content-type": "application/octet-stream",
        },
        url: package_.resolved,
      },
    });
    seeded++;
  }
  return { missing, seeded };
}

async function runSelfTest(npmCliPath) {
  const root = mkdtempSync(join(tmpdir(), "dufs-npm-cache-test-"));
  try {
    const source = join(root, "source");
    const destination = join(root, "destination");
    const cache = loadCacache(npmCliPath);
    const resolved =
      "https://registry.npmjs.org/example/-/example-1.0.0.tgz";
    const integrity =
      "sha512-7iawQ7pGZgF6vfdOQ8v3f0PpwPH3Wj9XQ3lV6WQxOQjB" +
      "h6jv+h+Qv5xQz4o2v8V10I8c4v0tY+v4hX8VvW8JYQ==";
    const data = Buffer.from("private npm cache fixture\n");
    const actual = await cache.put(
      join(source, "_cacache"),
      `make-fetch-happen:request-cache:${resolved}`,
      data,
      {
        metadata: {
          url: resolved,
          resHeaders: { "content-type": "application/octet-stream" },
        },
      },
    );
    const lock = {
      lockfileVersion: 3,
      packages: {
        "": {},
        "node_modules/example": {
          integrity: String(actual),
          resolved,
        },
      },
    };
    const seeded = await seedCache(lock, source, destination, cache);
    if (seeded.seeded !== 1 || seeded.missing !== 0) {
      throw new Error("npm cache self-test seeded the wrong entry count");
    }
    const copied = await cache.get(
      join(destination, "_cacache"),
      `make-fetch-happen:request-cache:${resolved}`,
    );
    if (!copied.data.equals(data)) {
      throw new Error("npm cache self-test changed tarball bytes");
    }

    lock.packages["node_modules/example"].integrity = integrity;
    let rejectedMismatch = false;
    try {
      await seedCache(lock, source, join(root, "mismatch"), cache);
    } catch {
      rejectedMismatch = true;
    }
    if (!rejectedMismatch) {
      throw new Error("npm cache self-test accepted mismatched integrity");
    }

    lock.packages["node_modules/example"] = {
      integrity: String(actual),
      resolved: "file:///tmp/example.tgz",
    };
    let rejectedLocal = false;
    try {
      await seedCache(lock, source, join(root, "local"), cache);
    } catch {
      rejectedLocal = true;
    }
    if (!rejectedLocal) {
      throw new Error("npm cache self-test accepted a local dependency");
    }
  } finally {
    rmSync(root, { force: true, recursive: true });
  }
  process.stdout.write("private npm cache seeding self-test passed\n");
}
