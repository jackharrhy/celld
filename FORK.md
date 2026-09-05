# Jack's Celld fork

This repository follows `denoland/celld` and carries a local object-store option
for standalone applications. `fork.json` records the tested upstream revision,
fork release version and Rust toolchain. [Radio](https://github.com/jackharrhy/radio)
and [Worldview](https://github.com/jackharrhy/worldview) use this mode.

The fork also carries two R2 compatibility repairs found by Radio's live-runtime
tests: full reads omit the optional range record, and new R2 envelope metadata
uses the Azure-safe `celld_r2` key while still reading the older `celld-r2` key.

## Consume a release

Successful main builds publish `ghcr.io/jackharrhy/celld:latest` for Linux amd64.
Application Dockerfiles copy `/usr/local/bin/celld` from that image and refresh
their base images when building. Applications publish their own `:latest` images;
the normal `infra update HOST` and `infra refresh HOST` workflow deploys them.
A fork update reaches an application on its next image build.

Commit tags (`sha-<full-fork-commit>`), optional `fork-*` release tags and image
digests remain available for debugging or restoring an older version. They do
not require a manual promotion step. The image also includes
`/usr/local/bin/celld-store-copy` for the one-time offline backend migration.
Applications need neither a Rust build nor a local experiment checkout.

Image labels record the fork commit, fork version and upstream revision.
`celld --version` retains the upstream package version and is not sufficient to
identify this fork.

## Standalone local storage

Use the same `--bucket sqlite:///absolute/path/objects.sqlite3` (or `CELLD_BUCKET`)
for normal runtime, deploy, diagnose and operator commands. The path is literal:
URI escapes, remote authorities, queries and fragments are rejected. There is
one fleet per database, without a bucket prefix. Local storage runs without the
managed control plane or cloud credentials/endpoints. It is independent of
`celld dev` and its cleanup lifecycle.

Deploy the Worker before starting the runtime, as for a cloud bucket. Put the
object database and `CELLD_WATCH` replica directory in separate subdirectories
of one persistent host volume. Set `CELLD_DURABILITY=bucket`; keep the internal
listener on loopback and expose only the public listener through the application
proxy. A lifetime `.runtime.lock` sidecar rejects a second runtime or migration
using the same local authority. Deploy and diagnostic commands may still open
the database. Never remove the lock file while a process could hold it.

The supported topology is one runtime on one host, using a persistent local
filesystem with working SQLite locking and fsync. Sharing a WAL database over a
network filesystem or copying it between active hosts does not create a fleet.
Machine/disk loss requires a backup restore.

The store retains SQLite transactions for conditional writes and atomic object
publication. Large objects use bounded chunks inside SQLite. Existing dev-store
inline objects remain readable, but the new chunked format cannot be opened by
older Celld builds. Preserve pre-upgrade backups when changing formats.

## Memory admission after large local writes

Release `0.4.1-jh.2` fixes a failure found during the final Radio container test:
with one CPU, 1 GiB memory and no swap, a 1 GiB upload filled the cgroup with
inactive filesystem cache. The previous hard-pressure rule used the full cgroup
charge and refused the upload's final Durable Object activation despite modest
process RSS and working-set usage. Host-process qualification had not exposed
that container-specific failure.

The hard-pressure metric now uses the greater of process RSS and the cgroup
working set, retaining allocator memory and active kernel charges. It excludes
inactive file cache using the existing telemetry calculation; missing or invalid
statistics fall back to the full charge. Ordinary limits, the 95% hard watermark,
and hysteresis remain enabled. Linux documents why a network-to-file workload
can fill available memory without needing it to operate in its
[cgroup memory guidance](https://docs.kernel.org/admin-guide/cgroup-v2.html#usage-guidelines).

The corrected release passed the constrained 1 GiB upload, restart and
state/media checks. Keep this regression harness available when changing storage
or memory behavior; ordinary application rollouts use their normal CI checks.

## Backup and recovery

For this initial deployment, stop the application runtime and all operators,
then back up the complete object-store directory (database, WAL/SHM when present)
and replica directory together. Do not copy only a live `objects.sqlite3` file.
Also retain application configuration/secrets and the exact application image
digest privately. Check free space before importing large audio collections.

Restore while the application is stopped. To prove the object store is the
authority, rehearse recovery into a fresh replica directory before live cutover.
Retain old runtime directories for recovery evidence. Restoring a backup loses
changes accepted after that backup. The migration document covers moving a
quiescent namespace between backends, including the rollback boundary.

## Follow upstream

Keep `upstream` pointed at `https://github.com/denoland/celld.git`. Fetch upstream
main and merge it into a dedicated sync branch based on this fork's main. Review
the remaining diff against that upstream revision, remove fixes now supplied
upstream, and update `fork.json` to the new upstream revision and fork version.
Resolve conflicts in that branch; never rewrite published release tags.

The Docker build runs workspace tests, clippy and storage/migration checks before
publishing. Application CI also exercises its real-Celld smoke.
Upstream's private suites are not included in its public checkout, so passing
the shipped tests alone is not application qualification. A successful sync
does not automatically update running applications.

Upstream contribution preparation is optional. The original release workflow
is gated to Denoland's repository; fork releases use `fork.yml` and our registry.
