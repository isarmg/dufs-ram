import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const projectRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const workflowPath = resolve(
  projectRoot,
  ".github",
  "workflows",
  "release-binary.yml",
);
const dependencyAuditWorkflowPath = resolve(
  projectRoot,
  ".github",
  "workflows",
  "dependency-audit.yml",
);
const qualityGatePath = resolve(projectRoot, "scripts", "check.sh");

function requireMatch(source, pattern, message) {
  if (!pattern.test(source)) {
    throw new Error(message);
  }
}

function jobBlock(source, name) {
  const marker = `  ${name}:\n`;
  const start = source.indexOf(marker);
  if (start === -1) {
    throw new Error(`missing ${name} job`);
  }
  const rest = source.slice(start + marker.length);
  const nextJob = /^  [0-9A-Za-z_]+:\n/mu.exec(rest);
  return source.slice(start, nextJob ? start + marker.length + nextJob.index : undefined);
}

function checkYankedAuditPreparation(source, label) {
  const fetch = source.indexOf("cargo fetch --locked");
  const audit = source.indexOf("cargo audit --deny yanked");
  if (fetch === -1 || audit === -1 || fetch > audit) {
    throw new Error(`${label} must fetch the locked graph before auditing yanked crates`);
  }
}

function isolatedEdgePolicyBlock(source) {
  const startMarker = "run npm run test:frontend\n";
  const endMarker = "run npm audit --audit-level=high\n";
  const start = source.indexOf(startMarker);
  if (start === -1 || source.indexOf(startMarker, start + 1) !== -1) {
    throw new Error("local quality gate must have one required browser-matrix entry");
  }
  const blockStart = start + startMarker.length;
  const end = source.indexOf(endMarker, blockStart);
  if (end === -1 || source.indexOf(endMarker, end + 1) !== -1) {
    throw new Error("local quality gate must have one npm audit entry");
  }
  return source.slice(blockStart, end);
}

function checkIsolatedEdgePolicy(source) {
  const block = isolatedEdgePolicyBlock(source);
  const expectedPolicy = /^if \[\[ "\$\{DUFS_ISOLATED_QUALITY_GATE:-\}" == "1" \]\]; then\n  printf '[^\n]+'\nelif command -v microsoft-edge >\/dev\/null 2>&1 \|\| command -v microsoft-edge-stable >\/dev\/null 2>&1; then\n  run npm run test:frontend:edge\nelse\n  printf '[^\n]+'\nfi\n$/u;
  if (!expectedPolicy.test(block)) {
    throw new Error(
      "isolated release gate must skip unpinned host Edge before optional discovery",
    );
  }
}

function checkWorkflow(source) {
  if (source.includes("\r") || !source.endsWith("\n")) {
    throw new Error("release workflow must use LF and end with a newline");
  }

  const verify = jobBlock(source, "verify_build");
  const publish = jobBlock(source, "publish");
  requireMatch(
    verify,
    /    permissions:\n      actions: read\n      contents: read\n/u,
    "verify_build must be read-only",
  );
  requireMatch(
    publish,
    /    permissions:\n      contents: write\n    steps:/u,
    "publish must have only contents:write",
  );
  if ((source.match(/      contents: write\n/gu) ?? []).length !== 1) {
    throw new Error("contents:write must appear in exactly one job");
  }

  requireMatch(
    verify,
    /uses: actions\/checkout@[0-9a-f]{40} # v7\.0\.1/u,
    "read-only build must check out a commit-pinned source",
  );
  if (publish.includes("actions/checkout") || publish.includes("scripts/")) {
    throw new Error("publish must not check out or invoke repository code");
  }
  const publishUses = [...publish.matchAll(/^        uses: (.+)$/gmu)]
    .map(match => match[1]);
  const expectedDownload =
    "actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c # v8.0.1";
  if (
    publishUses.length !== 1 ||
    publishUses[0] !== expectedDownload
  ) {
    throw new Error("publish may use only the pinned artifact downloader");
  }
  for (const use of source.matchAll(/^        uses: (.+)$/gmu)) {
    requireMatch(
      use[1],
      /^[0-9A-Za-z_.-]+\/[0-9A-Za-z_.-]+@[0-9a-f]{40} # v[0-9]+\.[0-9]+\.[0-9]+$/u,
      `action is not pinned to a full commit: ${use[1]}`,
    );
  }

  const requiredVerifyFragments = [
    "timeout-minutes: 240",
    "dependency-audit.yml 'dependency audit'",
    "formal-release-e2e.yml 'formal release package E2E'",
    '.headSha == $sha',
    '.headBranch == $ref',
    '.conclusion == "success"',
    "node scripts/extract-release-notes.mjs",
    "actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a",
    "release_artifact_id:",
    "release_input_digest:",
  ];
  for (const fragment of requiredVerifyFragments) {
    if (!verify.includes(fragment)) {
      throw new Error(`verify_build is missing: ${fragment}`);
    }
  }

  const requiredPublishFragments = [
    "artifact-ids: ${{ needs.verify_build.outputs.release_artifact_id }}",
    "digest-mismatch: error",
    "EXPECTED_RELEASE_INPUT_DIGEST: ${{ needs.verify_build.outputs.release_input_digest }}",
    "validate_release_metadata",
    "validate_draft_asset_subset",
    "require_complete_matching_draft",
    "gh api --paginate --slurp",
    "release_assets_state",
    "verify_tag_target",
    "verify_tag_target || return 1",
    "gh release download",
    "sha256sum \"$remote_path\"",
    "gh release upload",
    '.state == "starter"',
    ".size == 0",
    '.state == "uploaded"',
    ".digest ] == [$asset_digest]",
    'releases/assets/${asset_id}',
    "gh api --include --method DELETE",
    '^HTTP/[0-9.]+[[:space:]]+204',
    "--notes-file \"$notes_path\"",
    "gh release edit \"$tag\" --draft=false",
  ];
  for (const fragment of requiredPublishFragments) {
    if (!publish.includes(fragment)) {
      throw new Error(`publish is missing: ${fragment}`);
    }
  }
  if (source.includes("--generate-notes")) {
    throw new Error("release notes must not be generated from a previous tag");
  }
  if (publish.includes("gh release delete") || publish.includes("--clobber")) {
    throw new Error("publish must not delete or clobber an existing release asset");
  }
  if (publish.includes("releaseAssets(")) {
    throw new Error("publish must enumerate assets through the paginated REST API");
  }
}

const workflow = readFileSync(workflowPath, "utf8");
const dependencyAuditWorkflow = readFileSync(
  dependencyAuditWorkflowPath,
  "utf8",
);
const qualityGate = readFileSync(qualityGatePath, "utf8");
try {
  checkWorkflow(workflow);
  checkYankedAuditPreparation(
    dependencyAuditWorkflow,
    "dependency audit workflow",
  );
  checkYankedAuditPreparation(qualityGate, "local quality gate");
  checkIsolatedEdgePolicy(qualityGate);
} catch (error) {
  const message = error instanceof Error ? error.message : String(error);
  process.stderr.write(`check-release-workflow: ${message}\n`);
  process.exit(1);
}

const edgePolicyBlock = isolatedEdgePolicyBlock(qualityGate);
const mutatedEdgePolicyBlock = edgePolicyBlock.replace(
  '== "1" ]]; then',
  '== "0" ]]; then',
);
if (mutatedEdgePolicyBlock === edgePolicyBlock) {
  process.stderr.write(
    "check-release-workflow: unable to construct isolated Edge policy fixture\n",
  );
  process.exit(1);
}
let rejectedEdgePolicyMutation = false;
try {
  checkIsolatedEdgePolicy(
    qualityGate.replace(edgePolicyBlock, mutatedEdgePolicyBlock),
  );
} catch {
  rejectedEdgePolicyMutation = true;
}
if (!rejectedEdgePolicyMutation) {
  process.stderr.write(
    "check-release-workflow: isolated Edge policy regression was accepted\n",
  );
  process.exit(1);
}

for (const [source, label] of [
  [
    dependencyAuditWorkflow.replace("cargo fetch --locked", "cargo fetch"),
    "dependency audit workflow",
  ],
  [
    qualityGate.replace("run cargo fetch --locked", "run cargo fetch"),
    "local quality gate",
  ],
]) {
  try {
    checkYankedAuditPreparation(source, label);
  } catch {
    continue;
  }
  process.stderr.write(
    "check-release-workflow: a yanked-audit preparation regression was accepted\n",
  );
  process.exit(1);
}

const mutationFixtures = [
  workflow.replace("      contents: write\n", "      contents: read\n"),
  workflow.replace(
    "actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c",
    "actions/download-artifact@main",
  ),
  workflow.replace("dependency-audit.yml 'dependency audit'", "missing-audit"),
  workflow.replace(
    "formal-release-e2e.yml 'formal release package E2E'",
    "missing-formal-release-e2e",
  ),
  workflow.replace("gh release download", "gh release view"),
  workflow.replaceAll('.state == "starter"', '.state == "uploaded"'),
  workflow.replaceAll(".size == 0", ".size > 0"),
  workflow.replace("gh api --paginate --slurp", "gh api"),
  workflow.replace("gh api --include --method DELETE", "gh api --method DELETE"),
  workflow.replace(
    '^HTTP/[0-9.]+[[:space:]]+204',
    '^HTTP/[0-9.]+[[:space:]]+200',
  ),
  workflow.replaceAll("verify_tag_target || return 1", "verify_tag_target"),
];
for (const mutated of mutationFixtures) {
  try {
    checkWorkflow(mutated);
  } catch {
    continue;
  }
  process.stderr.write(
    "check-release-workflow: a security regression fixture was accepted\n",
  );
  process.exit(1);
}

process.stdout.write("release workflow static checks passed\n");
