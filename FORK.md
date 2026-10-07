# Jack's Celld fork

This repository follows `denoland/celld` and carries a local object-store option
for standalone applications. `fork.json` records the tested upstream revision,
fork release version and Rust toolchain. [Radio](https://github.com/jackharrhy/radio)
and [Worldview](https://github.com/jackharrhy/worldview) use this mode.

The fork retains the R2 full-read range repair found by Radio's live-runtime
tests: full reads omit the optional range record. Upstream 0.6.2 retains Azure
metadata normalization and reads both `celld_r2` and `celld-r2` metadata from
earlier fork releases.

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

## Upgrade from 0.4.1 to 0.5.1

Release `0.5.1-jh.1` follows upstream `42269c121c989c65c0638ab01f368baf18a5f0df`.
It retains the production SQLite backend, chunked object format, runtime lock,
offline copy tool, SQLite 3.51.3 and reclaimable-cache admission policy. Upstream
adds SQLite cache limits, C allocator trimming and recovery improvements. Memory
admission also counts upstream's container reservations.

This upgrade changes the engine's alarm-discovery format. Stop traffic, deploy
writers and the old runtime/supervisor, wait for its leases to expire, and back
up the complete stopped object store and node state before running the new
binary. Preserve node names, addresses and replica directories. Deployment or
startup upgrades the wake index automatically. A local migration takes the same
exclusive guard as runtime startup and offline copying; ordinary deployments
against an already-upgraded store remain possible while the runtime is online.

Do not restart a 0.4.1 binary against upgraded storage. Rollback requires restoring
the complete stopped pre-upgrade backup, losing subsequent writes unless saved
separately. See upstream `docs/guarantees.md` for the migration protocol. An image
rollback alone is insufficient.

This release does not claim to fix all lease timeouts under storage stalls. The
local store can wait up to 30 seconds on a SQLite writer, longer than the default
10-second lease TTL; fencing remains enabled and must not be weakened to conceal
an unhealthy storage path. Qualify application upgrades with the constrained
1 GiB upload, old-state migration, empty-replica crash recovery and lease-contention
checks in addition to the public Rust suites.

## Upgrade from 0.5.1 to 0.6.2

Release `0.6.2-jh.1` follows upstream `90b43017241f81189453d326d05948f388b34652`.
The production `sqlite://` backend, chunked local objects, offline copy tool,
runtime secret overrides, reclaimable-cache admission policy, and the R2
full-read range repair remain fork features. Upstream reorganized R2 metadata
handling into a shared module; the repair now lives only at the get response.

Upstream 0.6.0 gives facets their own replicated SQLite files and migrates
the previous root-embedded facet images on first open. A fleet using fleet
durability must stop every node before this upgrade; bucket durability can use
a rolling update. The standalone SQLite deployment uses bucket durability,
but still needs a stopped, complete backup of both object store and replica
state before first use. Do not publish or deploy this fork release until its
storage, migration, crash recovery, and application smokes pass.

## Runtime secrets

For standalone deployments (SQLite and cloud buckets), this fork retains `CELLD_VAR_<NAME>` and
`CELLD_VARS_FILE` overrides removed in upstream 0.5. Runtime environment values
override file entries, which override manifest variables. These values are read
when loading the trusted application and are not written into its deployment
manifest or image. All Workers in that application share the existing trust
boundary; this is not per-tenant secret isolation. Managed control-plane deployments reject these overrides. Other upstream removed-setting validation remains
in place.

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
