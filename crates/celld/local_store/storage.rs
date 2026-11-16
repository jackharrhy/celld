// Copyright 2026 Deno Land Inc. Apache-2.0 license.

use super::*;

// No individual BLOB approaches SQLite's default 1,000,000,000-byte limit.
// cache_size is per connection; bound it as well as the application buffers.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS objects (
  key TEXT PRIMARY KEY, body BLOB NOT NULL, etag INTEGER NOT NULL,
  modified_ms INTEGER NOT NULL, attributes TEXT NOT NULL,
  size INTEGER, content_id INTEGER
);
CREATE TABLE IF NOT EXISTS store_sequence (
  singleton INTEGER PRIMARY KEY CHECK (singleton = 1), next_etag INTEGER NOT NULL
);
INSERT OR IGNORE INTO store_sequence(singleton,next_etag) VALUES(1,1);
CREATE TABLE IF NOT EXISTS local_uploads (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  state INTEGER NOT NULL, touched_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS local_uploads_gc ON local_uploads(state,touched_ms);
CREATE TABLE IF NOT EXISTS local_parts (
  upload INTEGER NOT NULL, part INTEGER NOT NULL, size INTEGER NOT NULL,
  offset INTEGER NOT NULL DEFAULT -1, PRIMARY KEY(upload,part)
);
CREATE INDEX IF NOT EXISTS local_parts_offset ON local_parts(upload,offset);
CREATE TABLE IF NOT EXISTS local_chunks (
  upload INTEGER NOT NULL, part INTEGER NOT NULL, chunk INTEGER NOT NULL,
  body BLOB NOT NULL, PRIMARY KEY(upload,part,chunk)
);";

impl LocalStore {
    pub(crate) fn open(database: impl AsRef<FsPath>) -> object_store::Result<Self> {
        let store = Self {
            database: database.as_ref().to_path_buf(),
        };
        if let Some(parent) = store
            .database
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
        {
            // This backend is deliberately tied to the host's local filesystem.
            #[allow(clippy::disallowed_methods)]
            std::fs::create_dir_all(parent).map_err(db_error)?;
        }
        let mut connection = store.connect()?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(db_error)?;
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        let version: i64 = tx
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(db_error)?;
        if version > 2 {
            return Err(message_error(format!(
                "local store format {version} is newer than this Celld supports"
            )));
        }
        tx.execute_batch(SCHEMA).map_err(db_error)?;
        let columns = {
            let mut statement = tx.prepare("PRAGMA table_info(objects)").map_err(db_error)?;
            let columns = statement
                .query_map([], |r| r.get::<_, String>(1))
                .map_err(db_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(db_error)?;
            columns
        };
        if !columns.iter().any(|c| c == "size") {
            tx.execute_batch(
                "ALTER TABLE objects ADD COLUMN size INTEGER;
                              ALTER TABLE objects ADD COLUMN content_id INTEGER;",
            )
            .map_err(db_error)?;
        }
        tx.execute_batch(
            "CREATE INDEX IF NOT EXISTS objects_content ON objects(content_id);
                          PRAGMA user_version = 2;",
        )
        .map_err(db_error)?;
        tx.commit().map_err(db_error)?;
        store.cleanup_after_write();
        Ok(store)
    }

    pub(super) fn connect(&self) -> object_store::Result<Connection> {
        let connection = Connection::open(&self.database).map_err(db_error)?;
        connection
            .busy_timeout(Duration::from_secs(30))
            .map_err(db_error)?;
        connection
            .execute_batch(
                "PRAGMA synchronous=FULL; PRAGMA cache_size=-2048;
                                  PRAGMA temp_store=FILE;",
            )
            .map_err(db_error)?;
        Ok(connection)
    }

    pub(super) fn put_inline(
        &self,
        key: &str,
        body: &[u8],
        options: &PutOptions,
    ) -> object_store::Result<PutResult> {
        let attributes = encode_attributes(&options.attributes)?;
        let mut connection = self.connect()?;
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        check_mode(&tx, key, &options.mode)?;
        let old = content_id(&tx, key)?;
        let etag = next_etag(&tx)?;
        tx.execute(
            "INSERT INTO objects(key,body,etag,modified_ms,attributes,size,content_id)
                    VALUES(?1,?2,?3,?4,?5,?6,NULL)
                    ON CONFLICT(key) DO UPDATE SET body=excluded.body,etag=excluded.etag,
                    modified_ms=excluded.modified_ms,attributes=excluded.attributes,
                    size=excluded.size,content_id=NULL",
            params![
                key,
                body,
                etag,
                crate::asyncrt::wall_ms(),
                attributes,
                body.len() as i64
            ],
        )
        .map_err(db_error)?;
        retire(&tx, old)?;
        tx.commit().map_err(db_error)?;
        self.cleanup_after_write();
        Ok(put_result(etag))
    }

    pub(super) fn delete_sync(&self, key: &str) -> object_store::Result<()> {
        let mut connection = self.connect()?;
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        let old = content_id(&tx, key)?;
        tx.execute("DELETE FROM objects WHERE key=?1", [key])
            .map_err(db_error)?;
        retire(&tx, old)?;
        tx.commit().map_err(db_error)?;
        self.cleanup_after_write();
        Ok(())
    }

    pub(super) fn copy_sync(&self, from: &str, to: &str, create: bool) -> object_store::Result<()> {
        let mut connection = self.connect()?;
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        // Check the source before the destination, including copy-to-self.
        metadata(&tx, from)?;
        check_mode(
            &tx,
            to,
            &if create {
                PutMode::Create
            } else {
                PutMode::Overwrite
            },
        )?;
        let old = content_id(&tx, to)?;
        let etag = next_etag(&tx)?;
        tx.execute(
            "INSERT INTO objects(key,body,etag,modified_ms,attributes,size,content_id)
                    SELECT ?1,body,?2,?3,attributes,size,content_id FROM objects WHERE key=?4
                    ON CONFLICT(key) DO UPDATE SET body=excluded.body,etag=excluded.etag,
                    modified_ms=excluded.modified_ms,attributes=excluded.attributes,
                    size=excluded.size,content_id=excluded.content_id",
            params![to, etag, crate::asyncrt::wall_ms(), from],
        )
        .map_err(db_error)?;
        retire(&tx, old)?;
        tx.commit().map_err(db_error)?;
        self.cleanup_after_write();
        Ok(())
    }

    pub(super) fn cleanup_after_write(&self) {
        // Publication already committed: cleanup failure must not turn a success
        // into an error that prompts the caller to retry a conditional operation.
        let _ = self.collect_garbage();
    }

    pub(super) fn collect_garbage(&self) -> object_store::Result<()> {
        let mut connection = self.connect()?;
        // Reclamation is optional work after publication. Never wait behind
        // another writer, and never drain an arbitrarily large retired object
        // before acknowledging a lease update or other successful write.
        connection.busy_timeout(Duration::ZERO).map_err(db_error)?;
        let cutoff = crate::asyncrt::wall_ms().saturating_sub(24 * 60 * 60 * 1000);
        let id: Option<i64> = connection
            .query_row(
                "SELECT id FROM local_uploads WHERE state=2
             UNION ALL SELECT id FROM local_uploads WHERE state=0 AND touched_ms<?1 LIMIT 1",
                [cutoff],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_error)?;
        let Some(id) = id else {
            return Ok(());
        };
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        // Recheck expiry under the writer lock: an active uploader may have
        // progressed or published after the read-only candidate query.
        tx.execute(
            "UPDATE local_uploads SET state=2 WHERE id=?1 AND state=0 AND touched_ms<?2",
            params![id, cutoff],
        )
        .map_err(db_error)?;
        let retired: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM local_uploads WHERE id=?1 AND state=2)",
                [id],
                |r| r.get(0),
            )
            .map_err(db_error)?;
        if retired {
            tx.execute(
                "DELETE FROM local_chunks WHERE rowid IN
                        (SELECT rowid FROM local_chunks WHERE upload=?1 LIMIT 2)",
                [id],
            )
            .map_err(db_error)?;
            let left: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM local_chunks WHERE upload=?1)",
                    [id],
                    |r| r.get(0),
                )
                .map_err(db_error)?;
            if !left {
                tx.execute("DELETE FROM local_parts WHERE upload=?1", [id])
                    .map_err(db_error)?;
                tx.execute("DELETE FROM local_uploads WHERE id=?1", [id])
                    .map_err(db_error)?;
            }
        }
        tx.commit().map_err(db_error)
    }
}

pub(super) const META_COLUMNS: &str =
    "key,COALESCE(size,length(body)),etag,modified_ms,attributes,content_id";

pub(super) fn row_metadata(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredObject> {
    Ok(StoredObject {
        key: row.get(0)?,
        size: row.get::<_, i64>(1)? as u64,
        etag: row.get(2)?,
        modified_ms: row.get(3)?,
        attributes: row.get(4)?,
        content_id: row.get(5)?,
    })
}

pub(super) fn metadata(connection: &Connection, key: &str) -> object_store::Result<StoredObject> {
    connection
        .query_row(
            &format!("SELECT {META_COLUMNS} FROM objects WHERE key=?1"),
            [key],
            row_metadata,
        )
        .optional()
        .map_err(db_error)?
        .ok_or_else(|| not_found(key))
}

pub(super) fn content_id(connection: &Connection, key: &str) -> object_store::Result<Option<i64>> {
    Ok(connection
        .query_row("SELECT content_id FROM objects WHERE key=?1", [key], |r| {
            r.get::<_, Option<i64>>(0)
        })
        .optional()
        .map_err(db_error)?
        .flatten())
}

pub(super) fn retire(connection: &Connection, id: Option<i64>) -> object_store::Result<()> {
    if let Some(id) = id {
        connection
            .execute(
                "UPDATE local_uploads SET state=2 WHERE id=?1
            AND NOT EXISTS(SELECT 1 FROM objects WHERE content_id=?1)",
                [id],
            )
            .map_err(db_error)?;
    }
    Ok(())
}

pub(super) fn check_mode(
    connection: &Connection,
    key: &str,
    mode: &PutMode,
) -> object_store::Result<()> {
    let current = connection
        .query_row("SELECT etag FROM objects WHERE key=?1", [key], |r| {
            r.get::<_, i64>(0)
        })
        .optional()
        .map_err(db_error)?;
    match mode {
        PutMode::Overwrite => Ok(()),
        PutMode::Create if current.is_some() => Err(already_exists(key)),
        PutMode::Create => Ok(()),
        PutMode::Update(version) => {
            if current.is_none() || version.e_tag != current.map(|e| e.to_string()) {
                Err(precondition(key))
            } else {
                Ok(())
            }
        }
    }
}

pub(super) fn next_etag(connection: &Connection) -> object_store::Result<i64> {
    connection
        .query_row(
            "UPDATE store_sequence SET next_etag=next_etag+1
        WHERE singleton=1 RETURNING next_etag-1",
            [],
            |r| r.get(0),
        )
        .map_err(db_error)
}

pub(super) fn put_result(etag: i64) -> PutResult {
    PutResult {
        e_tag: Some(etag.to_string()),
        version: None,
    }
}
