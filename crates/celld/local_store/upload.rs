// Copyright 2026 Deno Land Inc. Apache-2.0 license.

use super::*;
use futures_util::{future::BoxFuture, FutureExt as _};
use storage::{check_mode, content_id, next_etag, put_result, retire};

// Matches the multipart limit used by object-store providers, while bounding
// the metadata needed to lay out parts at publication. Parts need not arrive in order.
const MAX_PARTS: usize = 10_000;

impl LocalStore {
    pub(super) fn begin_upload(&self) -> object_store::Result<i64> {
        let connection = self.connect()?;
        connection
            .execute(
                "INSERT INTO local_uploads(state,touched_ms) VALUES(0,?1)",
                [crate::asyncrt::wall_ms()],
            )
            .map_err(db_error)?;
        Ok(connection.last_insert_rowid())
    }

    pub(super) fn write_part(
        &self,
        id: i64,
        part: usize,
        payload: PutPayload,
    ) -> object_store::Result<()> {
        let size = i64::try_from(payload.content_length()).map_err(db_error)?;
        let mut connection = self.connect()?;
        let mut buffer = Vec::with_capacity(CHUNK_SIZE);
        let mut chunk = 0i64;
        for bytes in payload {
            let mut remaining = bytes.as_ref();
            while !remaining.is_empty() {
                let count = (CHUNK_SIZE - buffer.len()).min(remaining.len());
                buffer.extend_from_slice(&remaining[..count]);
                remaining = &remaining[count..];
                if buffer.len() == CHUNK_SIZE {
                    write_chunk(&mut connection, id, part, chunk, &buffer)?;
                    self.cleanup_after_write();
                    chunk += 1;
                    buffer.clear();
                }
            }
        }
        if !buffer.is_empty() {
            write_chunk(&mut connection, id, part, chunk, &buffer)?;
            self.cleanup_after_write();
        }
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        touch_upload(&tx, id)?;
        tx.execute(
            "INSERT INTO local_parts(upload,part,size) VALUES(?1,?2,?3)",
            params![id, part as i64, size],
        )
        .map_err(db_error)?;
        tx.commit().map_err(db_error)
    }

    pub(super) fn publish_upload(
        &self,
        id: i64,
        parts: usize,
        key: &str,
        options: &PutOptions,
    ) -> object_store::Result<PutResult> {
        let attributes = encode_attributes(&options.attributes)?;
        let mut connection = self.connect()?;
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        touch_upload(&tx, id)?;
        let sizes = {
            let mut stmt = tx
                .prepare("SELECT part,size FROM local_parts WHERE upload=?1 ORDER BY part")
                .map_err(db_error)?;
            let sizes = stmt
                .query_map([id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
                .map_err(db_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(db_error)?;
            sizes
        };
        if sizes.len() != parts
            || sizes
                .iter()
                .enumerate()
                .any(|(index, (part, _))| *part != index as i64)
        {
            return Err(message_error(
                "multipart completion requires every part to finish successfully",
            ));
        }
        let mut size = 0i64;
        for (part, part_size) in sizes {
            tx.execute(
                "UPDATE local_parts SET offset=?1 WHERE upload=?2 AND part=?3",
                params![size, id, part],
            )
            .map_err(db_error)?;
            size = size
                .checked_add(part_size)
                .ok_or_else(|| message_error("object size overflow"))?;
        }
        check_mode(&tx, key, &options.mode)?;
        let old = content_id(&tx, key)?;
        let etag = next_etag(&tx)?;
        tx.execute(
            "INSERT INTO objects(key,body,etag,modified_ms,attributes,size,content_id)
            VALUES(?1,X'',?2,?3,?4,?5,?6) ON CONFLICT(key) DO UPDATE SET
            body=excluded.body,etag=excluded.etag,modified_ms=excluded.modified_ms,
            attributes=excluded.attributes,size=excluded.size,content_id=excluded.content_id",
            params![key, etag, crate::asyncrt::wall_ms(), attributes, size, id],
        )
        .map_err(db_error)?;
        tx.execute("UPDATE local_uploads SET state=1 WHERE id=?1", [id])
            .map_err(db_error)?;
        retire(&tx, old)?;
        tx.commit().map_err(db_error)?;
        self.cleanup_after_write();
        Ok(put_result(etag))
    }

    pub(super) fn abort_upload(&self, id: i64) -> object_store::Result<()> {
        self.connect()?
            .execute(
                "UPDATE local_uploads SET state=2 WHERE id=?1 AND state=0",
                [id],
            )
            .map_err(db_error)?;
        self.cleanup_after_write();
        Ok(())
    }
}

fn touch_upload(connection: &Connection, id: i64) -> object_store::Result<()> {
    let changed = connection
        .execute(
            "UPDATE local_uploads SET touched_ms=?1 WHERE id=?2 AND state=0",
            params![crate::asyncrt::wall_ms(), id],
        )
        .map_err(db_error)?;
    if changed != 1 {
        return Err(message_error(
            "multipart upload was completed, aborted, or expired",
        ));
    }
    Ok(())
}

fn write_chunk(
    connection: &mut Connection,
    id: i64,
    part: usize,
    chunk: i64,
    body: &[u8],
) -> object_store::Result<()> {
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(db_error)?;
    touch_upload(&tx, id)?;
    tx.execute(
        "INSERT INTO local_chunks(upload,part,chunk,body) VALUES(?1,?2,?3,?4)",
        params![id, part as i64, chunk, body],
    )
    .map_err(db_error)?;
    tx.commit().map_err(db_error)
}

#[derive(Debug)]
pub(super) struct LocalUpload {
    store: LocalStore,
    id: i64,
    location: Path,
    attributes: Attributes,
    parts: usize,
    finished: bool,
}

impl LocalUpload {
    pub(super) fn new(store: LocalStore, id: i64, location: Path, attributes: Attributes) -> Self {
        Self {
            store,
            id,
            location,
            attributes,
            parts: 0,
            finished: false,
        }
    }
}

// Dropping or crashing an upload leaves only invisible staging rows. They expire
// after 24 hours without progress and are reclaimed in bounded transactions.
// Explicit abort makes them eligible immediately. No async task is spawned from
// Drop: uploads can be dropped while their runtime is shutting down.
#[async_trait]
impl MultipartUpload for LocalUpload {
    fn put_part(&mut self, data: PutPayload) -> BoxFuture<'static, object_store::Result<()>> {
        if self.finished || self.parts >= MAX_PARTS {
            return async {
                Err(message_error(
                    "multipart upload closed or part limit exceeded",
                ))
            }
            .boxed();
        }
        let part = self.parts;
        self.parts += 1;
        let store = self.store.clone();
        let id = self.id;
        async move {
            crate::asyncrt::blocking(move || store.write_part(id, part, data))
                .await
                .map_err(db_error)?
        }
        .boxed()
    }

    async fn complete(&mut self) -> object_store::Result<PutResult> {
        if self.finished {
            return Err(message_error("multipart upload already closed"));
        }
        let store = self.store.clone();
        let id = self.id;
        let parts = self.parts;
        let key = self.location.to_string();
        let options = PutOptions {
            attributes: self.attributes.clone(),
            ..Default::default()
        };
        let result =
            crate::asyncrt::blocking(move || store.publish_upload(id, parts, &key, &options))
                .await
                .map_err(db_error)?;
        if result.is_ok() {
            self.finished = true;
        }
        result
    }

    async fn abort(&mut self) -> object_store::Result<()> {
        if self.finished {
            return Ok(());
        }
        let store = self.store.clone();
        let id = self.id;
        crate::asyncrt::blocking(move || store.abort_upload(id))
            .await
            .map_err(db_error)??;
        self.finished = true;
        Ok(())
    }
}
