# Security policy

## Supported version

Security fixes are developed and reviewed in the current repository source
tree, but neither a repository `HEAD` nor a dirty worktree is automatically a
supported binary release.
For deployed binaries, only the latest release produced by the documented
exact-tag, checksum, and signature workflow is supported. Until such a release
exists, this repository makes no supported-binary claim. Older releases,
dirty worktrees, and downstream-modified builds receive no automatic security
updates.

## Reporting a vulnerability

Do not disclose a suspected vulnerability in a public issue, chat room, log,
or shared document. Reports about this fork should use its GitHub
[private vulnerability reporting form](https://github.com/isarmg/dufs-ram/security/advisories/new)
when that form is enabled. If it is unavailable, do not fall back to a public
issue; use a private security contact published by the fork maintainer or by
the organization that supplied the binary. A report confirmed to affect an
unmodified upstream release may also be coordinated through the upstream
[private form](https://github.com/sigoden/dufs/security/advisories/new).
Send the following information:

- affected version and the Git SHA printed by `dufs --version`;
- deployment topology and relevant configuration with passwords, PHC strings,
  cookies, CSRF tokens, private keys, hostnames, and personal paths removed;
- minimal reproduction steps and expected impact;
- whether exploitation is already suspected.

The fork maintainers target an acknowledgement within three business days,
an initial severity and scope assessment within seven business days, and a
private status update at least every seven days until remediation or closure.
These are response targets rather than a promise that every issue can be fixed
on that schedule. The operator must preserve logs, restrict backend network
access, and contact the relevant maintainer through a private reporting
channel before any coordinated publication. A distributor must publish its
actual monitored contact address alongside the binary; this repository does
not promise that a public upstream issue is confidential.

## Operational security boundary

Dufs is intended for trusted accounts in a personal or controlled network. It
must run as a dedicated unprivileged user behind an HTTPS gateway, with the
backend port restricted to that gateway. All authenticated users can manage
the entire shared root. Do not expose this build as a multi-tenant service for
mutually untrusted users.

Forwarded client and scheme headers are ignored unless the immediate TCP peer
matches an explicitly configured `--trusted-proxy` / `trusted-proxies` IP or
CIDR. That allowlist is an operator assertion, not proxy authentication.
Trusting `127.0.0.1/32` assumes other local processes are trusted or prevented
from reaching the backend by operating-system isolation; a loopback bind alone
does not distinguish nginx from another local process. A remote gateway needs
a narrow source allowlist plus a private network, firewall, or equivalent ACL.
Without explicit trust, rate limiting uses the TCP peer and HTTPS-origin writes
through a cleartext gateway fail closed because the external scheme cannot be
verified.

The shared root must be writable only by the Dufs service account while the
service is running. Path leases and identity checks coordinate this process;
they do not make the namespace an isolation boundary against a shell, host
process, or other writer with equivalent credentials. A destination expected
to be missing is published with `RENAME_NOREPLACE`, so a late occupant is not
overwritten, and the resulting destination is checked against the pinned
source descriptor before success is reported. Replacing an existing target is
different: Dufs rechecks source and destination identities and then performs an
ordinary rename; this is not a kernel directory-entry compare-and-replace, and
an external writer can still exchange either name in that narrow interval.
Purge moves each final
removal candidate to a random quarantine/disposal name and verifies the pinned
descriptor again before unlinking it; an identity or final-removal anomaly
quarantines the whole trash root instead of resuming at an old cursor. A
malicious same-UID process which observes random work names with inotify and
races the final verification-to-unlink window remains outside the supported
threat boundary. Excluding that actor requires an inaccessible private work
directory or operating-system identity isolation, not another pathname check.

The release builder and the release signer are separate security roles.
Production signing should run under a different operating-system account, on
an isolated signing host, or in an HSM-backed service which never executes
project source, dependency build scripts, Cargo, rustc, Node.js, or SBOM tools.
The repository release script deliberately opens a file-based key only after
all such tools have exited, but processes running under the same UID can still
read one another's readable files and inspect process metadata. Late opening
prevents accidental descriptor inheritance; it is not an isolation boundary
against malicious code with the signing account's UID.

File-based release signing is also fail-closed on key strength. The supported
set is Ed25519, Ed448, RSA with at least 3072 bits, and ECDSA on prime256v1,
secp384r1, or secp521r1. DSA, weak RSA, unapproved EC curves, encryption-only
keys, and algorithms whose type or strength cannot be determined are rejected
before a signature can be published. This allowlist is a repository release
policy, not a substitute for independent key custody, rotation, and revocation.

Release source identity must be checked independently of mutable repository
metadata. The documented release workflow rejects Git replacement refs,
legacy grafts and private repository attributes, reads the commit through an
isolated object-store facade whose minimal generated local configuration is
checksum-locked while system/global configuration is disabled, and verifies
each extracted tree against the declared commit. Tracked symlinks, submodules,
special entries, and non-regular extracted entries are rejected. The workflow
also runs the complete quality gate in a verified, Git-metadata-free commit
archive with a sanitized environment and private Cargo, npm, build, and
temporary state. Cargo dependencies are vendored before offline checks; npm
cache entries are admitted only after matching lockfile HTTPS locations and
SHA-512 integrity. The gate requires cargo-audit 0.22.2. A host RustSec
database is reusable only when its origin is the canonical RustSec repository,
HEAD equals the fetched revision, its physical `FETCH_HEAD` is no more than
seven days old and no more than 300 seconds ahead of the current clock, and a
full physical, Git-metadata, mode, and content validation succeeds. Alternates,
unsafe source/Git entries, symlinks,
submodules, special files, untracked paths, and tracked content or mode drift
are rejected. A valid database is cloned without hard links and sealed by its
revision, fetch epoch, index checksum, and generated-config checksum. An
invalid, stale, or missing host database is refreshed in private state with a
dummy lockfile before any project or dependency code runs; lack of network
fails closed. The release then performs a sealed
`cargo audit --db ... --no-fetch --no-yanked` pre-audit. The isolated quality
gate requires that same database through `DUFS_QUALITY_AUDIT_DB`, and
`scripts/check.sh` audits it before other project or dependency steps. The seal
is revalidated after the pre-audit; after the complete gate, both the seal and
freshness are revalidated before the quality database is discarded. The
accepted advisory revision and fetch epoch are
recorded in the signed package environment manifest; internal index/config
seal checksums are validation inputs, not manifest fields. Missing npm packages and `npm audit` can
still require controlled network access; environment isolation is not itself
proof of a fully offline quality gate. A separate snapshot index then verifies
tracked content, modes, and unexpected non-ignored paths. That quality tree is
discarded before a fresh extraction supplies the signed build. The exact clean
tag/source is rechecked after the gate, before signing, and before publication.
A signed tag alone does not neutralize local Git replacement or attribute
rules.

Release consumers should verify both the normalized CycloneDX SBOM and
`THIRD_PARTY_LICENSES.txt` through the package `SHA256SUMS`. The notice is
generated only from vendored, reachable non-development dependencies. Every
package must declare a non-empty reviewed SPDX `license` expression; a
`license_file` supplies upstream text only and cannot replace that expression
or act as a classification fallback. Expressions are parsed as SPDX syntax and
must offer a complete approved permissive branch; every declared or
conventional license/notice candidate must be a non-empty UTF-8, no-follow
regular file inside that dependency's own vendored source. The project license
is never fallback text for a dependency.
SBOM normalization accepts only an exact 40- or 64-character lowercase
hexadecimal source revision and prevents local build-path leakage, but is not
a complete CycloneDX schema validation.

The package must also contain
`RUST-STANDARD-LIBRARY-COPYRIGHT.html`, copied from the pinned Rust 1.97.1
sysroot only after its regular-file, containment, and reviewed SHA-256 checks
pass. It, the project license, dependency notice, and SBOM are all covered by
the package `SHA256SUMS`; an unknown toolchain without a reviewed standard
library notice digest is not releasable.

An authenticated upload will not copy setuid/setgid bits or any
`security.*`/`trusted.*` extended attribute onto a replacement inode; such a
target is rejected. If those privileged attributes must be preserved, perform
the change through a separately controlled privileged administration process.
Non-privileged attributes are also fail-closed and bounded: the name list is
limited to 64 KiB and 1024 entries, each value to 64 KiB, and the combined
index, NUL-terminated names, and exact-sized values to 1 MiB. Values are sized
before allocation rather than receiving a fixed 64 KiB buffer per attribute.

SQLite `upload_sessions` is the sole upload-state authority; the shared root
contains no JSON upload-state record. Lookup is keyed by the authenticated
owner digest and UUID. Stored target/stage paths are treated as untrusted bytes
and must pass canonical root-relative resolution and exact binding checks.
Owner-scoped absence is returned as not seen, while a malformed row, invalid
stored path, or SQLite failure fails closed as a state-storage error rather
than being silently downgraded to absence. A partial running checkpoint also
opens the stage through the exact writable no-follow path used by PATCH, then
checks that descriptor is a regular, single-link file whose identity matches
the durable checkpoint and whose length reaches the durable offset. A
full-offset running record remains an ambiguity barrier even when its stage is
read-only, already renamed, missing, or otherwise abnormal; it is never
downgraded to not-seen merely because the stage cannot be reopened.
The response-only `not-started` state means that the current request, whose ID
and length were parsed, stopped before any upload mutation; it does not prove
that the same ID has no older owner-scoped record. A retry must therefore query
the old ID before choosing PATCH, a new ID, or no replay. The server acquires
the path lease and upload permit before tracked route metadata. A fresh PUT
then checks durable upload/purge obligations for the target path and its
descendants, under the same upload deadline and before registering or creating
this upload mutation. Conflict, state-store failure, and inspection timeout
return bound `409`, `503`, and `408` not-started responses respectively. A full
upload admission returns `429 not-started` without reading or changing any
older record. The subsequently tracked upload task may still perform read-only
session, target, metadata, and space preparation. Before its first filesystem
or upload-state mutation, it atomically races that boundary against the total
deadline. If the deadline closes the boundary first, the server aborts the
task; no later continuation can cross the closed boundary, and the response is
bound `408 request_timeout`, `not-started`, and `retry`. An unhandled timeout
from read-only preparation has the same contract; other unhandled pre-boundary
I/O is bound `503 upload_precommit_failed`, `not-started`, and `retry`. If the
task crosses the mutation boundary first, a later outer deadline or unhandled
error is instead `unknown` with `query_upload`. Existing checkpoints remain
authoritative in every case, so even a definite not-started response does not
authorize choosing a replay mode without the owner-scoped HEAD query.

Potential credentials and file contents must never be attached to a report.
Rotate any secret that was disclosed during investigation.
