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

function stepBlock(source, name) {
  const marker = `      - name: ${name}\n`;
  const start = source.indexOf(marker);
  if (start === -1 || source.indexOf(marker, start + marker.length) !== -1) {
    throw new Error(`expected exactly one ${name} step`);
  }
  const rest = source.slice(start + marker.length);
  const nextStep = /^      - name: /mu.exec(rest);
  const end = nextStep ? start + marker.length + nextStep.index : source.length;
  return {
    body: source.slice(start, end),
    bodyStart: start,
    bodyEnd: end,
  };
}

function escapeRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/gu, "\\$&");
}

function shellFunctionBlock(source, name) {
  const definitionPattern = new RegExp(
    `^([ \\t]*)${escapeRegExp(name)}\\(\\)[ \\t]*\\{[ \\t]*\\n`,
    "gmu",
  );
  const definitions = [...source.matchAll(definitionPattern)];
  if (definitions.length !== 1) {
    throw new Error(`expected exactly one ${name} shell function`);
  }

  const definition = definitions[0];
  const bodyStart = definition.index + definition[0].length;
  const closingPattern = new RegExp(
    `^${escapeRegExp(definition[1])}\\}[ \\t]*(?:\\n|$)`,
    "gmu",
  );
  closingPattern.lastIndex = bodyStart;
  const closing = closingPattern.exec(source);
  if (!closing) {
    throw new Error(`missing closing brace for ${name} shell function`);
  }
  return {
    body: source.slice(bodyStart, closing.index),
    bodyStart,
    bodyEnd: closing.index,
  };
}

function requireExclusiveShellLine(source, command, message) {
  const matches = source
    .split("\n")
    .map((line, index) => ({ index, line }))
    .filter(({ line }) => line.trim() === command);
  if (matches.length !== 1) {
    throw new Error(message);
  }
  return matches[0].index;
}

function mutateExclusiveBlockLine(
  source,
  block,
  command,
  replacement,
  label,
) {
  const lines = block.body.split("\n");
  const matches = lines
    .map((line, index) => ({ index, line }))
    .filter(({ line }) => line.trim() === command);
  if (matches.length !== 1) {
    throw new Error(`unable to construct ${label} mutation fixture`);
  }

  const { index, line } = matches[0];
  if (replacement === null) {
    lines.splice(index, 1);
  } else {
    const indentation = /^[ \\t]*/u.exec(line)?.[0] ?? "";
    lines[index] = `${indentation}${replacement}`;
  }
  return (
    source.slice(0, block.bodyStart) +
    lines.join("\n") +
    source.slice(block.bodyEnd)
  );
}

function mutateExclusiveFunctionLine(source, name, command, replacement) {
  return mutateExclusiveBlockLine(
    source,
    shellFunctionBlock(source, name),
    command,
    replacement,
    name,
  );
}

function mutateExclusiveStepLine(source, name, command, replacement) {
  return mutateExclusiveBlockLine(
    source,
    stepBlock(source, name),
    command,
    replacement,
    name,
  );
}

function replaceExactlyOnce(source, expected, replacement, label) {
  const start = source.indexOf(expected);
  if (
    start === -1 ||
    source.indexOf(expected, start + expected.length) !== -1
  ) {
    throw new Error(`unable to construct ${label} mutation fixture`);
  }
  return (
    source.slice(0, start) +
    replacement +
    source.slice(start + expected.length)
  );
}

function moveStepAfter(source, movingName, targetName) {
  const moving = stepBlock(source, movingName);
  const target = stepBlock(source, targetName);
  if (moving.bodyStart >= target.bodyStart) {
    throw new Error(`unable to move ${movingName} after ${targetName}`);
  }

  const movingSource = source.slice(moving.bodyStart, moving.bodyEnd);
  const withoutMoving =
    source.slice(0, moving.bodyStart) + source.slice(moving.bodyEnd);
  const shiftedTarget = stepBlock(withoutMoving, targetName);
  return (
    withoutMoving.slice(0, shiftedTarget.bodyEnd) +
    movingSource +
    withoutMoving.slice(shiftedTarget.bodyEnd)
  );
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

function checkFreshReleaseAudit(source, verify) {
  requireExclusiveShellLine(
    source,
    'CARGO_AUDIT_VERSION: "0.22.2"',
    "release workflow must pin cargo-audit 0.22.2 exactly once",
  );

  const install = stepBlock(
    verify,
    "Install the fixed Rust toolchain and cargo-audit",
  );
  const setupNode = stepBlock(verify, "Use the fixed Node.js release");
  const audit = stepBlock(verify, "Audit dependencies for this release attempt");
  const build = stepBlock(verify, "Build the release binary");
  if (
    install.bodyStart >= audit.bodyStart ||
    setupNode.bodyStart >= audit.bodyStart ||
    audit.bodyStart >= build.bodyStart
  ) {
    throw new Error(
      "fresh dependency audit must follow fixed tool setup and precede the build",
    );
  }

  for (const command of [
    'cargo +"$RUST_TOOLCHAIN" install cargo-audit \\',
    '--version "$CARGO_AUDIT_VERSION" \\',
    "--locked",
    'test "$(cargo audit --version)" = \\',
    '"cargo-audit-audit $CARGO_AUDIT_VERSION"',
  ]) {
    requireExclusiveShellLine(
      install.body,
      command,
      `cargo-audit setup is missing an active exclusive line: ${command}`,
    );
  }

  const cargoFetch = requireExclusiveShellLine(
    audit.body,
    "cargo fetch --locked",
    "fresh release audit must fetch the locked Cargo graph",
  );
  const cargoAudit = requireExclusiveShellLine(
    audit.body,
    "cargo audit --deny yanked",
    "fresh release audit must check RustSec advisories and yanked crates",
  );
  const npmInstall = requireExclusiveShellLine(
    audit.body,
    "npm ci --ignore-scripts --no-audit --no-fund",
    "fresh release audit must install the locked npm graph without scripts",
  );
  const npmAudit = requireExclusiveShellLine(
    audit.body,
    "npm audit --audit-level=high",
    "fresh release audit must check current high-severity npm advisories",
  );
  if (cargoFetch >= cargoAudit || npmInstall >= npmAudit) {
    throw new Error("fresh release audits must prepare each locked graph first");
  }
}

function checkReleaseAttemptBinding(verify, publish) {
  const audit = stepBlock(verify, "Audit dependencies for this release attempt");
  const upload = stepBlock(verify, "Transfer the verified release inputs");
  const record = stepBlock(verify, "Record the successful verification attempt");
  if (audit.bodyStart >= record.bodyStart || upload.bodyStart >= record.bodyStart) {
    throw new Error("verification attempt must be recorded after audit and upload");
  }
  requireExclusiveShellLine(
    verify,
    "verification_run_attempt: ${{ steps.record_verification_attempt.outputs.run_attempt }}",
    "verify_build must expose the attempt captured by its successful step",
  );
  requireExclusiveShellLine(
    record.body,
    "id: record_verification_attempt",
    "verification attempt recorder must have the expected step ID",
  );
  requireExclusiveShellLine(
    record.body,
    `printf 'run_attempt=%s\\n' "$GITHUB_RUN_ATTEMPT" >> "$GITHUB_OUTPUT"`,
    "verification attempt recorder must persist the executed run attempt",
  );

  const guard = stepBlock(
    publish,
    "Require verification from this workflow attempt",
  );
  const download = stepBlock(publish, "Download the exact build artifact");
  const stepsMarker = "    steps:\n";
  const stepsStart = publish.indexOf(stepsMarker);
  if (
    stepsStart === -1 ||
    guard.bodyStart !== stepsStart + stepsMarker.length ||
    guard.bodyStart >= download.bodyStart
  ) {
    throw new Error("publish must validate the run attempt in its first step");
  }
  requireExclusiveShellLine(
    guard.body,
    "VERIFIED_RUN_ATTEMPT: ${{ needs.verify_build.outputs.verification_run_attempt }}",
    "publish must consume the attempt captured by verify_build",
  );
  for (const command of [
    'if [[ ! "$VERIFIED_RUN_ATTEMPT" =~ ^[1-9][0-9]*$ || \\',
    '! "$GITHUB_RUN_ATTEMPT" =~ ^[1-9][0-9]*$ || \\',
    '"$VERIFIED_RUN_ATTEMPT" != "$GITHUB_RUN_ATTEMPT" ]]',
    "exit 1",
  ]) {
    requireExclusiveShellLine(
      guard.body,
      command,
      `publish attempt guard is missing an active exclusive line: ${command}`,
    );
  }
}

function checkWorkflow(source) {
  if (source.includes("\r") || !source.endsWith("\n")) {
    throw new Error("release workflow must use LF and end with a newline");
  }

  const verify = jobBlock(source, "verify_build");
  const publish = jobBlock(source, "publish");
  const matchingDraft = shellFunctionBlock(publish, "require_matching_draft");
  checkFreshReleaseAudit(source, verify);
  checkReleaseAttemptBinding(verify, publish);
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
    "printf 'Source commit: `%s`\\n' \"$GITHUB_SHA\"",
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
    "release(tagName:$tag){databaseId tagName",
    ".data.repository.release.databaseId",
    '($id | type) == "number"',
    "$id > 0",
    "$id <= 2147483647",
    "($id | floor) == $id",
    'if [[ ! "$release_id" =~ ^[1-9][0-9]*$ ]]; then',
    '"repos/${GH_REPO}/releases/${release_id}"',
    '(.id | tostring) == $release_id',
    ".data.repository.release.tagName == $tag",
    ".tag_name == $tag",
    "verify_tag_target",
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
  requireExclusiveShellLine(
    matchingDraft.body,
    "verify_tag_target || return 1",
    "require_matching_draft must exclusively verify the tag target",
  );
  if (source.includes("--generate-notes")) {
    throw new Error("release notes must not be generated from a previous tag");
  }
  if (publish.includes("gh release delete") || publish.includes("--clobber")) {
    throw new Error("publish must not delete or clobber an existing release asset");
  }
  if (publish.includes("releaseAssets(")) {
    throw new Error("publish must enumerate assets through the paginated REST API");
  }
  if (publish.includes('releases/tags/${tag}')) {
    throw new Error("publish must resolve draft releases through their database ID");
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

const freshAuditStepName = "Audit dependencies for this release attempt";
const verificationAttemptStepName =
  "Record the successful verification attempt";
const publishAttemptGuardStepName =
  "Require verification from this workflow attempt";
const missingFreshCargoAudit = mutateExclusiveStepLine(
  workflow,
  freshAuditStepName,
  "cargo audit --deny yanked",
  null,
);
const commentedFreshCargoAudit = mutateExclusiveStepLine(
  workflow,
  freshAuditStepName,
  "cargo audit --deny yanked",
  "# cargo audit --deny yanked",
);
const unlockedFreshCargoFetch = mutateExclusiveStepLine(
  workflow,
  freshAuditStepName,
  "cargo fetch --locked",
  "cargo fetch",
);
const missingFreshNpmAudit = mutateExclusiveStepLine(
  workflow,
  freshAuditStepName,
  "npm audit --audit-level=high",
  null,
);
const commentedFreshNpmAudit = mutateExclusiveStepLine(
  workflow,
  freshAuditStepName,
  "npm audit --audit-level=high",
  "# npm audit --audit-level=high",
);
const missingVerificationAttemptCapture = mutateExclusiveStepLine(
  workflow,
  verificationAttemptStepName,
  `printf 'run_attempt=%s\\n' "$GITHUB_RUN_ATTEMPT" >> "$GITHUB_OUTPUT"`,
  null,
);
const contextOnlyVerificationAttempt = replaceExactlyOnce(
  workflow,
  "verification_run_attempt: ${{ steps.record_verification_attempt.outputs.run_attempt }}",
  "verification_run_attempt: ${{ github.run_attempt }}",
  "context-only verification attempt",
);
const unboundPublishAttempt = mutateExclusiveStepLine(
  workflow,
  publishAttemptGuardStepName,
  "VERIFIED_RUN_ATTEMPT: ${{ needs.verify_build.outputs.verification_run_attempt }}",
  "VERIFIED_RUN_ATTEMPT: ${{ github.run_attempt }}",
);
const invertedPublishAttemptGuard = mutateExclusiveStepLine(
  workflow,
  publishAttemptGuardStepName,
  '"$VERIFIED_RUN_ATTEMPT" != "$GITHUB_RUN_ATTEMPT" ]]',
  '"$VERIFIED_RUN_ATTEMPT" == "$GITHUB_RUN_ATTEMPT" ]]',
);
const commentedPublishAttemptGuard = mutateExclusiveStepLine(
  workflow,
  publishAttemptGuardStepName,
  '"$VERIFIED_RUN_ATTEMPT" != "$GITHUB_RUN_ATTEMPT" ]]',
  '# "$VERIFIED_RUN_ATTEMPT" != "$GITHUB_RUN_ATTEMPT" ]]',
);
const latePublishAttemptGuard = moveStepAfter(
  workflow,
  publishAttemptGuardStepName,
  "Download the exact build artifact",
);

const missingMatchingDraftTagCheck = mutateExclusiveFunctionLine(
  workflow,
  "require_matching_draft",
  "verify_tag_target || return 1",
  null,
);
const commentedMatchingDraftTagCheck = mutateExclusiveFunctionLine(
  workflow,
  "require_matching_draft",
  "verify_tag_target || return 1",
  "# verify_tag_target || return 1",
);

const mutationFixtures = [
  missingFreshCargoAudit,
  commentedFreshCargoAudit,
  unlockedFreshCargoFetch,
  missingFreshNpmAudit,
  commentedFreshNpmAudit,
  missingVerificationAttemptCapture,
  contextOnlyVerificationAttempt,
  unboundPublishAttempt,
  invertedPublishAttemptGuard,
  commentedPublishAttemptGuard,
  latePublishAttemptGuard,
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
  workflow.replace("databaseId tagName", "tagName"),
  workflow.replace(
    ".data.repository.release.databaseId",
    ".data.repository.release.id",
  ),
  workflow.replace(
    '($id | type) == "number"',
    '($id | type) == "string"',
  ),
  workflow.replace("$id > 0", "$id >= 0"),
  workflow.replace("$id <= 2147483647", "$id <= 0"),
  workflow.replace("($id | floor) == $id", "($id | floor) != $id"),
  workflow.replace(
    'if [[ ! "$release_id" =~ ^[1-9][0-9]*$ ]]; then',
    'if [[ ! "$release_id" =~ ^[0-9]+$ ]]; then',
  ),
  workflow.replace(
    '"repos/${GH_REPO}/releases/${release_id}"',
    '"repos/${GH_REPO}/releases/tags/${tag}"',
  ),
  workflow.replace(
    "(.id | tostring) == $release_id",
    "(.id | tostring) != $release_id",
  ),
  workflow.replace(
    ".data.repository.release.tagName == $tag",
    ".data.repository.release.tagName != $tag",
  ),
  workflow.replace(".tag_name == $tag", ".tag_name != $tag"),
  workflow.replace("gh api --include --method DELETE", "gh api --method DELETE"),
  workflow.replace(
    '^HTTP/[0-9.]+[[:space:]]+204',
    '^HTTP/[0-9.]+[[:space:]]+200',
  ),
  missingMatchingDraftTagCheck,
  commentedMatchingDraftTagCheck,
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
