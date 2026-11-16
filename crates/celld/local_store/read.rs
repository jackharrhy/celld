// Copyright 2026 Deno Land Inc. Apache-2.0 license.

use super::*;

struct ReadSnapshot {
    connection: Connection,
    object: StoredObject,
    position: u64,
    end: u64,
}

impl LocalStore {
    pub(super) fn get_snapshot(
        &self,
        key: &str,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        let connection = self.connect()?;
        connection
            .execute_batch("BEGIN DEFERRED")
            .map_err(db_error)?;
        // This query establishes the snapshot before any writer can replace the
        // metadata, so every subsequent chunk belongs to the same object version.
        let object = storage::metadata(&connection, key)?;
        let meta = object_meta(&object)?;
        options.check_preconditions(&meta)?;
        let range = match options.range {
            Some(range) => range.as_range(object.size).map_err(db_error)?,
            None => 0..object.size,
        };
        let attributes = decode_attributes(&object.attributes)?;
        let payload = if options.head {
            // Release the snapshot immediately. HEAD never selects payload data.
            GetResultPayload::Stream(stream::empty().boxed())
        } else {
            let read = ReadSnapshot {
                connection,
                object,
                position: range.start,
                end: range.end,
            };
            let body = stream::try_unfold(read, |mut read| async move {
                crate::asyncrt::blocking(move || {
                    if read.position == read.end {
                        return Ok(None);
                    }
                    let bytes = read.next()?;
                    Ok(Some((bytes, read)))
                })
                .await
                .map_err(db_error)?
            });
            GetResultPayload::Stream(body.boxed())
        };
        Ok(GetResult {
            payload,
            meta,
            range,
            attributes,
        })
    }
}

impl ReadSnapshot {
    fn next(&mut self) -> object_store::Result<Bytes> {
        let length = (self.end - self.position).min(CHUNK_SIZE as u64);
        let (bytes, expected): (Vec<u8>, u64) = if let Some(id) = self.object.content_id {
            let (part, offset, size): (i64, i64, i64) = self
                .connection
                .query_row(
                    "SELECT part,offset,size FROM local_parts
                 WHERE upload=?1 AND offset<=?2 AND size>0 ORDER BY offset DESC LIMIT 1",
                    params![id, self.position as i64],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .map_err(db_error)?;
            let relative = self.position - offset as u64;
            if relative >= size as u64 {
                return Err(message_error("invalid object chunk layout"));
            }
            let chunk = relative / CHUNK_SIZE as u64;
            let within = relative % CHUNK_SIZE as u64;
            let length = length
                .min(CHUNK_SIZE as u64 - within)
                .min(size as u64 - relative);
            let bytes=self.connection.query_row(
                "SELECT substr(body,?1,?2) FROM local_chunks WHERE upload=?3 AND part=?4 AND chunk=?5",
                params![within as i64+1,length as i64,id,part,chunk as i64],|r|r.get(0)).map_err(db_error)?;
            (bytes, length)
        } else {
            // Existing development databases keep inline BLOBs. SQLite substr
            // reads only the requested bytes; never materialize a legacy object.
            let bytes = self
                .connection
                .query_row(
                    "SELECT substr(body,?1,?2) FROM objects WHERE key=?3",
                    params![self.position as i64 + 1, length as i64, self.object.key],
                    |r| r.get(0),
                )
                .map_err(db_error)?;
            (bytes, length)
        };
        if bytes.len() as u64 != expected {
            return Err(message_error("truncated object chunk"));
        }
        self.position += expected;
        Ok(bytes.into())
    }
}
