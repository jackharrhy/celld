# Local object-store format and operation

Copying a large legacy inline object uses SQLite `INSERT ... SELECT` and can
materialize that legacy value. Use the streaming migration tool before relying
on bounded-copy memory; newly written chunked objects share generations.

The single-node `sqlite:///absolute/path/objects.sqlite3` backend uses SQLite WAL
on a local filesystem. The workspace pins rusqlite 0.39.0 with bundled SQLite
3.51.3, including the SQLite WAL-reset corruption fix. Every writer connection
sets `synchronous=FULL`; the filesystem and device must honor successful syncs.
Do not put the database on NFS or share it between machines.

Objects at most 1 MiB may remain inline. Larger writes and multipart uploads are
staged as BLOBs at most 1 MiB each in the same database. Uploads remain invisible
until one transaction allocates an ETag and publishes all metadata and the content
generation. Conditional create/update checks run in that transaction. This
supports Radio's 1 GiB limit without approaching SQLite's 1,000,000,000-byte limit
on an individual BLOB or buffering the entire upload. Multipart supports 10,000
parts, in call order even if the part futures complete out of order.

Reads return chunks at most 1 MiB from one SQLite read snapshot; HEAD selects only
metadata. Overwriting or deleting an object cannot change an already opened
stream. A slow or abandoned stream holds old WAL pages until the stream is dropped.
Allow disk headroom for active reads, concurrent staging, and replacements. Copy
shares committed chunk generations; removal reclaims them only after the last
object reference disappears. Keyset listing uses SQLite's ordered key index and
bounded batches; delimiter groups are skipped by indexed seeks. Listing requests
are individually consistent, but pagination is not a fleet-wide snapshot under
concurrent mutation.

Explicit abort and object retirement make chunks eligible for cleanup immediately.
An upload abandoned by process death or dropping its handle expires after 24 hours
without a successful chunk write. Expiration is checked under the same writer lock
as upload progress, so a late part cannot resurrect reclaimed data. Startup and
successful writes perform one best-effort cleanup batch of at most two payload
chunks, using a zero busy timeout. Staged chunk writes also trigger cleanup so
future uploads reclaim old data faster than they add it. Cleanup never waits for
another writer before acknowledging a committed lease or object write. An idle
database can retain abandoned/retired chunks until subsequent opens or writes.
Deleted SQLite pages are reusable space; the database file does not automatically
shrink. Do not manually delete staging rows or WAL files while Celld is running.

Opening an existing inline development database adds nullable size/content columns
and preserves keys, attributes, ETags and the persistent allocation sequence. Legacy
inline values remain readable through bounded `substr` reads. New databases have
`user_version=2`; newer unknown format versions are rejected. **An older Celld
binary must not open a version-2 database:** older code does not understand chunk
references. Rollback uses a separately preserved source store, not an old binary
pointed at this database. Keep the Azure source snapshot until the full Radio
migration and restart tests pass.
