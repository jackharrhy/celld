// Copyright 2026 Deno Land Inc. Apache-2.0 license.

use super::*;
use futures_util::TryStreamExt as _;

pub(super) fn directory_prefix(prefix: Option<&Path>) -> String {
    let prefix = prefix.map(Path::as_ref).unwrap_or_default();
    if prefix.is_empty() {
        String::new()
    } else {
        format!("{prefix}/")
    }
}

// UTF-8 and SQLite's BINARY text collation have the same scalar ordering.
// The exclusive upper bound makes even a delimiter with millions of children
// a pair of indexed seeks, rather than a scan of the children.
fn prefix_end(prefix: &str) -> Option<String> {
    let mut end = prefix.to_owned();
    while let Some(last) = end.pop() {
        let next = last as u32 + 1;
        if next > 0x10ffff {
            continue;
        }
        end.push(char::from_u32(if next == 0xd800 { 0xe000 } else { next }).unwrap());
        return Some(end);
    }
    None
}

impl LocalStore {
    pub(super) fn list_stream(
        &self,
        prefix: Option<&Path>,
        after: Option<String>,
    ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        let state = (self.clone(), directory_prefix(prefix), after, false);
        stream::try_unfold(state, |(store, prefix, after, done)| async move {
            if done {
                return Ok::<_, Error>(None);
            }
            let reader = store.clone();
            let query_prefix = prefix.clone();
            let objects = crate::asyncrt::blocking(move || {
                metadata_page(
                    &reader.connect()?,
                    &query_prefix,
                    after.as_deref(),
                    LIST_BATCH,
                )
            })
            .await
            .map_err(db_error)??;
            let done = objects.len() < LIST_BATCH;
            let after = objects.last().map(|o| o.key.clone());
            let objects = objects
                .iter()
                .map(object_meta)
                .collect::<object_store::Result<Vec<_>>>()?;
            Ok(Some((
                stream::iter(objects.into_iter().map(Ok)),
                (store, prefix, after, done),
            )))
        })
        .try_flatten()
        .boxed()
    }

    pub(super) fn page(
        &self,
        prefix: &str,
        options: PaginatedListOptions,
    ) -> object_store::Result<PaginatedListResult> {
        let connection = self.connect()?;
        // Every individual page has one consistent view. Across requests, the
        // key cursor intentionally provides the provider's usual weak listing
        // consistency under concurrent insertion/deletion.
        connection
            .execute_batch("BEGIN DEFERRED")
            .map_err(db_error)?;
        let mut after = options.page_token.or(options.offset);
        let delimiter = options.delimiter.filter(|d| !d.is_empty());
        let limit = options.max_keys.unwrap_or(1000);
        let mut result = ListResult {
            common_prefixes: Vec::new(),
            objects: Vec::new(),
        };
        if limit == 0 {
            return Ok(PaginatedListResult {
                result,
                page_token: None,
            });
        }
        let mut count = 0;
        loop {
            let Some(object) = metadata_page(&connection, prefix, after.as_deref(), 1)?.pop()
            else {
                return Ok(PaginatedListResult {
                    result,
                    page_token: None,
                });
            };
            if count == limit {
                return Ok(PaginatedListResult {
                    result,
                    page_token: after,
                });
            }
            let remainder = &object.key[prefix.len()..];
            let common = delimiter.as_ref().and_then(|d| {
                remainder
                    .find(d.as_ref())
                    .map(|i| format!("{prefix}{}{}", &remainder[..i], d))
            });
            if let Some(common) = common {
                result.common_prefixes.push(Path::parse(&common)?);
                // Advance to the last actual key in this group. This token is
                // also meaningful as a raw-key offset, like the original store.
                let upper = prefix_end(&common);
                let sql = if upper.is_some() {
                    "SELECT key FROM objects WHERE key>=?1 AND key<?2 ORDER BY key DESC LIMIT 1"
                } else {
                    "SELECT key FROM objects WHERE key>=?1 AND ?2 IS NULL ORDER BY key DESC LIMIT 1"
                };
                after = Some(
                    connection
                        .query_row(sql, params![common, upper], |r| r.get(0))
                        .map_err(db_error)?,
                );
            } else {
                result.objects.push(object_meta(&object)?);
                after = Some(object.key);
            }
            count += 1;
        }
    }
}

fn metadata_page(
    connection: &Connection,
    prefix: &str,
    after: Option<&str>,
    limit: usize,
) -> object_store::Result<Vec<StoredObject>> {
    let lower = after.map_or(prefix, |after| prefix.max(after));
    let upper = prefix_end(prefix);
    let upper_clause = if upper.is_some() {
        "key<?2"
    } else {
        "?2 IS NULL"
    };
    let sql = format!(
        "SELECT {} FROM objects WHERE key>=?1 AND {upper_clause}
                    AND (?3 IS NULL OR key>?3) ORDER BY key LIMIT ?4",
        storage::META_COLUMNS
    );
    let mut statement = connection.prepare(&sql).map_err(db_error)?;
    let objects = statement
        .query_map(
            params![lower, upper, after, limit as i64],
            storage::row_metadata,
        )
        .map_err(db_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(db_error)?;
    Ok(objects)
}
