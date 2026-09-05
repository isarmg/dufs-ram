import { spawnSync } from "node:child_process";
import { cpSync, lstatSync, mkdtempSync, rmSync } from "node:fs";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

// Vite interprets '#' in physical filenames as a URL fragment. Build the
// exact installed inputs in a fresh private, URL-safe scratch directory. The
// release output and its held-directory publication protocol stay unchanged.
const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const scratch = mkdtempSync("/tmp/dufs-platform-build-");
const output = join(root, "clients/web/dist");

try {
  for (const directory of ["clients", "clients/web", "node_modules"]) {
    const metadata = lstatSync(join(root, directory));
    if (!metadata.isDirectory() || metadata.isSymbolicLink()) {
      throw new Error(`Build input is not a real directory: ${directory}`);
    }
  }
  cpSync(join(root, "clients/web"), join(scratch, "clients/web"), {
    recursive: true,
    filter: source => relative(join(root, "clients/web"), source).split("/")[0] !== "dist",
  });
  cpSync(join(root, "node_modules"), join(scratch, "node_modules"), { recursive: true });
  for (const file of ["package.json", "vite.platform.config.mjs"]) {
    cpSync(join(root, file), join(scratch, file));
  }
  const result = spawnSync(process.execPath, [
    join(scratch, "node_modules/vite/bin/vite.js"),
    "build", "--config", "vite.platform.config.mjs",
  ], { cwd: scratch, env: process.env, stdio: "inherit" });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`Platform build failed: ${result.status ?? result.signal}`);

  const previous = lstatSync(output, { throwIfNoEntry: false });
  if (previous && (!previous.isDirectory() || previous.isSymbolicLink())) {
    throw new Error("Refusing to replace a non-directory or linked platform output");
  }
  // dist is generated output only; never remove source or dependency inputs.
  rmSync(output, { recursive: true, force: true });
  cpSync(join(scratch, "clients/web/dist"), output, { recursive: true, errorOnExist: true });
} finally {
  rmSync(scratch, { recursive: true, force: true });
}
