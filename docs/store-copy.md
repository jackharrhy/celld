# Offline migration between Azurite and SQLite

`celld-store-copy` transfers a complete Celld namespace through its object-store
interfaces. It supports `az://CONTAINER[/PREFIX]` and
`sqlite:///absolute/path/objects.sqlite3`, including the reverse direction. It
does not start, stop, deploy, or reconfigure an application. It never selects
particular cell tables or rewrites deployment, ownership, lease, or log JSON.

This is a maintenance-window tool. Stop application nodes, their restart
policies, background jobs, deploy tools, and every other writer to both
namespaces. `--quiesced` records that prerequisite in the command. The tool
holds the same exclusive local runtime lock as a SQLite-backed Celld node.
Azure has no equivalent fleet-wide lock in this tool. Large-object multipart
completion has no conditional-create option: another writer must not race it.

## Build

```sh
cargo +1.94.1 build --locked -p celld --bin celld-store-copy
```

The executable is `target/debug/celld-store-copy` unless `CARGO_TARGET_DIR` is
set. Use the same fork revision and compatible deployment/runtime version for
the migration and destination node. Application upgrades are a separate change.

## Prepare a source snapshot

1. Record the application's deployment version, binding names, fleet container
   and prefix, runtime configuration, important application counts, and a few
   known object/cell contents. Include completed audio uploads and their R2
   metadata in the application checks.
2. Stop ingress and other application work, then drain and stop the node. Keep
   the stopped runtime directories. Confirm the final durability barrier and
   relevant recovery state; process exit status zero alone is insufficient.
   In fleet durability mode, acknowledged writes can be retained in follower
   logs or bundles before becoming per-cell objects. Do not delete node records:
   their folded log fields identify data that recovery still needs.
3. Inspect current-version folded roots while the source store is available:

   ```sh
   celld-store-copy audit --source az://radio
   ```

   This reports every `nodes/*.json` root and fails for open, recovering,
   malformed, or unknown log states. A sealed or absent root is useful evidence;
   the audit does not inspect runtime disks or prove that every application ACK
   is present. Investigate an unsealed root through source-runtime recovery.
   Never manufacture a seal by editing the JSON.
   The audit is diagnostic; `copy` does not automatically reject historical
   roots. The operator must reconcile relevant roots with the source durability
   mode and completed application barriers before asserting `--quiesced`.
4. Gracefully stop Azurite after the application has stopped, then take a
   consistent backup of its complete data directory and the stopped runtime
   directories. Record backup hashes and preserve that backup. The default
   Azurite metadata store periodically saves state, so killing it immediately
   after successful PUTs is not a safe snapshot method.
5. Make the source available to the importer with application writers still
   stopped. An isolated working copy of the backup can serve these reads while
   the original backup remains untouched. Do not start the old application.

For an emulator, the tool uses Celld's normal environment, for example:

```sh
export AZURE_STORAGE_USE_EMULATOR=true
export AZURITE_BLOB_STORAGE_URL=http://127.0.0.1:11000
```

For Azure credentials, use the same accepted `AZURE_*` settings as the source
node. The CLI does not accept or write credentials into its manifest.

## Copy, verify, and resume

Use an empty destination database and a new manifest file. These paths are
examples; use the actual fleet namespace and an absolute SQLite filename.

```sh
celld-store-copy copy \
  --source az://radio \
  --destination sqlite:///srv/radio/local-store/objects.sqlite3 \
  --manifest /srv/radio/migration/source-manifest.json \
  --quiesced

celld-store-copy verify \
  --source az://radio \
  --destination sqlite:///srv/radio/local-store/objects.sqlite3 \
  --manifest /srv/radio/migration/source-manifest.json
```

The manifest's parent directory must exist. Before writing any destination
objects, the tool writes and fsyncs a manifest containing every logical key,
size, SHA-256, object attribute, source ETag/version, and source modification
time. The destination's existing objects are never accepted on size alone.

The tool transfers all keys in the specified scope: cells and LTX, node roots,
log bundles and recovery records, deployment pointers/manifests/modules, shared
asset bodies, queue attachments, and `r2/<bucket_name>/` objects. Provider prefixes
are removed and applied once. Canonical object-store paths remain canonical,
including percent signs and Unicode. Embedded logical references and binding
identities remain unchanged.

Each copied object is read back and checked. The final verification rereads
both namespaces, checks their exact keysets, hashes, sizes, and attributes, and
checks that the source generations stayed unchanged. Transfer buffers are
bounded by the 8 MiB upload part size plus the source stream's chunk. No object
is loaded whole solely because it is an audio file.

After an interrupted copy, keep both namespaces quiescent and use the same
manifest and specifications:

```sh
celld-store-copy copy \
  --source az://radio \
  --destination sqlite:///srv/radio/local-store/objects.sqlite3 \
  --manifest /srv/radio/migration/source-manifest.json \
  --quiesced --resume
```

Resume validates the source snapshot, refuses extra or mismatching destination
keys, verifies existing matching objects, and copies only missing objects. It
does not repair by deletion or overwrite. A crash while initially writing the
manifest can leave an invalid manifest, before any object transfer began; retain
it for diagnosis and use a fresh manifest only after establishing that the
destination is still empty. An interrupted multipart upload can leave staged
parts for backend cleanup, but only completed objects enter the copied keyset.

Exit status zero means the requested verification passed. JSON on stdout gives
counts and transferred bytes; progress goes to stderr. Keep both the manifest
and the command output with the backup. The tool does not resume a runtime or
declare the application ready.

## Metadata and application compatibility

The copy preserves object bytes and all attributes exposed by `object_store`:
content type, encoding, language, disposition, cache control, storage class, and
every user-metadata name/value. Unknown attribute types fail instead of being
dropped. R2's reserved `celld_r2` metadata (historically `celld-r2`) contains
custom metadata, checksums, cache expiry, and storage class; copying only bodies
would lose those values.
Provider-specific LTX timestamp metadata names are retained unchanged.

ETags, provider version IDs, and last-modified times are generated by the
destination. Celld's persisted ownership/log wire records do not embed their
transport CAS token, so they are copied verbatim and fresh processes read the
new tokens. R2 exposes these values to application code, including its
`uploaded` time and conditional operations. Audit any application-persisted
ETags, date-based object behavior, or provider URLs before cutover. The tool
does not preserve provider version history, blob tags not exposed by the GET
attribute API, account/container policies, credentials, or incomplete uploads.
Those are not silently declared equivalent.

If a provider adds or transforms attributes, exact verification fails. In
particular, a reverse transfer of newly created SQLite objects should be
rehearsed rather than assuming the destination's default headers are identical.
This fork writes its R2 envelope under Azure-compatible `celld_r2` and reads
both that name and the older `celld-r2`, retaining the legacy flat-metadata
fallback. Forward and reverse copy of the current envelope are rehearsed.
Historical local objects can still carry `celld-r2`; Azure rejects that
hyphenated metadata name, so an exact reverse copy of those objects fails.
The importer never silently renames or discards it. Such historical objects
need an explicitly reviewed compatibility conversion before reverse migration.

## Cutover and rollback

Keep old processes and their automatic restarts disabled. Start the destination
with a fresh runtime directory and process generation, unchanged deployment and
binding identities, and the new SQLite bucket configuration. Do not carry a
clean-reload marker or stale replica cache into the destination. Verify known
cell contents and R2 audio bytes/metadata through the actual application before
enabling normal work. SQLite and Azurite are separate authority histories; they
cannot fence application processes that are still serving the other copy.

Before the destination performs application or background mutations, the
preserved source is the rollback point. Once new writes or background jobs run,
switching back to the old snapshot loses those changes. A lossless rollback
requires another quiescent, verified copy from SQLite into a fresh Azure
namespace and fresh runtime startup. The reverse uses the same command with the
source and destination exchanged and a new manifest. Keep the original snapshot
until the destination and rollback procedure have been accepted.

## Seeded rehearsal

The default binary tests exercise SQLite copy, safe resume after partial data,
refusal of extra/mismatching objects, source drift, and the active-runtime lock:

```sh
cargo +1.94.1 test --locked -p celld --bin celld-store-copy
```

The opt-in Azure test requires an already-created **disposable, empty**
container named `celld-copy-test-NAME`. It owns only its `source/` and
`returned/` prefixes and removes its fixture objects afterwards:

```sh
AZURE_STORAGE_USE_EMULATOR=true \
AZURITE_BLOB_STORAGE_URL=http://127.0.0.1:11000 \
CELLD_STORE_COPY_TEST_AZURE=az://celld-copy-test-rehearsal/source \
cargo +1.94.1 test --locked -p celld --bin celld-store-copy \
  azure_local_azure_roundtrip -- --ignored --nocapture
```

The rehearsal copies Azure to SQLite and back, verifies resume and the separate
verification mode, and includes opaque deployment/root records, a zero-length
object, current `celld_r2` and legacy flat R2 metadata, a percent/Unicode key, and an
audio-sized object crossing the multipart boundary. The default SQLite test
separately verifies preservation of historical `celld-r2` envelopes as well.
The LTX-shaped fixture is opaque data for copy testing;
actual Celld/LTX recovery is a separate integration check.
