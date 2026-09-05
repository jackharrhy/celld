// Copyright 2026 Deno Land Inc. Apache-2.0 license.

//! A persistent object store for one machine, on a local filesystem.
//!
//! Metadata, ETag allocation and publication share one SQLite transaction.
//! Large payloads are staged in bounded chunks in the same database: unfinished
//! uploads are invisible, and publication does not copy or buffer their bodies.
//! Readers hold a WAL snapshot until their stream is dropped. Keep that lifetime
//! bounded: a slow reader can retain old pages in the WAL during concurrent writes.
//! The chunked format is backward readable, but cannot be opened by older Celld.

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::stream::{self, BoxStream};
use futures_util::StreamExt as _;
use object_store::list::{PaginatedListOptions, PaginatedListResult, PaginatedListStore};
use object_store::path::Path;
use object_store::{
    Attribute, AttributeValue, Attributes, Error, GetOptions, GetResult, GetResultPayload,
    ListResult, MultipartUpload, ObjectMeta, ObjectStore, PutMode, PutMultipartOptions, PutOptions,
    PutPayload, PutResult,
};
use rusqlite::{params, Connection, OptionalExtension as _, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path as FsPath, PathBuf};
use std::time::{Duration, SystemTime};

#[path = "local_store/listing.rs"]
mod listing;
#[path = "local_store/read.rs"]
mod read;
#[path = "local_store/storage.rs"]
mod storage;
#[path = "local_store/upload.rs"]
mod upload;

const STORE: &str = "celld local store";
const CHUNK_SIZE: usize = 1024 * 1024;
const LIST_BATCH: usize = 128;

#[derive(Clone, Debug)]
pub(crate) struct LocalStore {
    database: PathBuf,
}

#[derive(Debug)]
struct StoredObject {
    key: String,
    size: u64,
    etag: i64,
    modified_ms: i64,
    attributes: String,
    content_id: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredAttribute {
    kind: String,
    name: Option<String>,
    value: String,
}

impl fmt::Display for LocalStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "LocalStore({})", self.database.display())
    }
}

#[async_trait]
impl ObjectStore for LocalStore {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        options: PutOptions,
    ) -> object_store::Result<PutResult> {
        let store = self.clone();
        let key = location.to_string();
        crate::asyncrt::blocking(move || {
            if payload.content_length() <= CHUNK_SIZE {
                let body: Bytes = payload.into();
                return store.put_inline(&key, &body, &options);
            }
            let id = store.begin_upload()?;
            let result = store
                .write_part(id, 0, payload)
                .and_then(|()| store.publish_upload(id, 1, &key, &options));
            if result.is_err() {
                let _ = store.abort_upload(id);
            }
            result
        })
        .await
        .map_err(db_error)?
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        options: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        let store = self.clone();
        let id = crate::asyncrt::blocking(move || store.begin_upload())
            .await
            .map_err(db_error)??;
        Ok(Box::new(upload::LocalUpload::new(
            self.clone(),
            id,
            location.clone(),
            options.attributes,
        )))
    }

    async fn get_opts(
        &self,
        location: &Path,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        let store = self.clone();
        let key = location.to_string();
        crate::asyncrt::blocking(move || store.get_snapshot(&key, options))
            .await
            .map_err(db_error)?
    }

    async fn delete(&self, location: &Path) -> object_store::Result<()> {
        let store = self.clone();
        let key = location.to_string();
        crate::asyncrt::blocking(move || store.delete_sync(&key))
            .await
            .map_err(db_error)?
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.list_stream(prefix, None)
    }

    fn list_with_offset(
        &self,
        prefix: Option<&Path>,
        offset: &Path,
    ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.list_stream(prefix, Some(offset.to_string()))
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
        let prefix = listing::directory_prefix(prefix);
        let mut result = ListResult {
            common_prefixes: Vec::new(),
            objects: Vec::new(),
        };
        let mut page_token = None;
        loop {
            let page = self
                .list_paginated(
                    Some(&prefix),
                    PaginatedListOptions {
                        delimiter: Some("/".into()),
                        max_keys: Some(LIST_BATCH),
                        page_token,
                        ..Default::default()
                    },
                )
                .await?;
            result.common_prefixes.extend(page.result.common_prefixes);
            result.objects.extend(page.result.objects);
            page_token = page.page_token;
            if page_token.is_none() {
                break;
            }
        }
        Ok(result)
    }

    async fn copy(&self, from: &Path, to: &Path) -> object_store::Result<()> {
        let store = self.clone();
        let from = from.to_string();
        let to = to.to_string();
        crate::asyncrt::blocking(move || store.copy_sync(&from, &to, false))
            .await
            .map_err(db_error)?
    }

    async fn copy_if_not_exists(&self, from: &Path, to: &Path) -> object_store::Result<()> {
        let store = self.clone();
        let from = from.to_string();
        let to = to.to_string();
        crate::asyncrt::blocking(move || store.copy_sync(&from, &to, true))
            .await
            .map_err(db_error)?
    }
}

#[async_trait]
impl PaginatedListStore for LocalStore {
    async fn list_paginated(
        &self,
        prefix: Option<&str>,
        options: PaginatedListOptions,
    ) -> object_store::Result<PaginatedListResult> {
        let store = self.clone();
        let prefix = prefix.unwrap_or_default().to_owned();
        crate::asyncrt::blocking(move || store.page(&prefix, options))
            .await
            .map_err(db_error)?
    }
}
fn object_meta(object: &StoredObject) -> object_store::Result<ObjectMeta> {
    let modified = SystemTime::UNIX_EPOCH
        .checked_add(Duration::from_millis(object.modified_ms.max(0) as u64))
        .ok_or_else(|| message_error("the object timestamp is outside the system clock range"))?;
    Ok(ObjectMeta {
        location: Path::parse(&object.key)?,
        last_modified: modified.into(),
        size: object.size,
        e_tag: Some(object.etag.to_string()),
        version: None,
    })
}

fn encode_attributes(attributes: &Attributes) -> object_store::Result<String> {
    let mut stored = Vec::with_capacity(attributes.len());
    for (attribute, value) in attributes {
        let (kind, name) = match attribute {
            Attribute::ContentDisposition => ("content-disposition", None),
            Attribute::ContentEncoding => ("content-encoding", None),
            Attribute::ContentLanguage => ("content-language", None),
            Attribute::ContentType => ("content-type", None),
            Attribute::CacheControl => ("cache-control", None),
            Attribute::StorageClass => ("storage-class", None),
            Attribute::Metadata(name) => ("metadata", Some(name.as_ref().to_string())),
            _ => {
                return Err(message_error(
                    "the development store does not support this attribute",
                ))
            }
        };
        stored.push(StoredAttribute {
            kind: kind.to_string(),
            name,
            value: value.as_ref().to_string(),
        });
    }
    serde_json::to_string(&stored).map_err(db_error)
}

fn decode_attributes(encoded: &str) -> object_store::Result<Attributes> {
    let stored: Vec<StoredAttribute> = serde_json::from_str(encoded).map_err(db_error)?;
    let mut attributes = Attributes::with_capacity(stored.len());
    for stored in stored {
        let attribute = match stored.kind.as_str() {
            "content-disposition" => Attribute::ContentDisposition,
            "content-encoding" => Attribute::ContentEncoding,
            "content-language" => Attribute::ContentLanguage,
            "content-type" => Attribute::ContentType,
            "cache-control" => Attribute::CacheControl,
            "storage-class" => Attribute::StorageClass,
            "metadata" => Attribute::Metadata(stored.name.unwrap_or_default().into()),
            kind => return Err(message_error(format!("unknown stored attribute {kind:?}"))),
        };
        attributes.insert(attribute, AttributeValue::from(stored.value));
    }
    Ok(attributes)
}

#[cfg(all(test, celld_internal_tests))]
mod internal_tests {
    include!(env!("CELLD_INTERNAL_LOCAL_STORE_TESTS"));
}

fn db_error(error: impl fmt::Display) -> Error {
    message_error(error.to_string())
}

fn message_error(message: impl Into<String>) -> Error {
    Error::Generic {
        store: STORE,
        source: Box::new(std::io::Error::other(message.into())),
    }
}

fn not_found(path: &str) -> Error {
    Error::NotFound {
        path: path.to_string(),
        source: Box::new(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "the object does not exist",
        )),
    }
}

fn already_exists(path: &str) -> Error {
    Error::AlreadyExists {
        path: path.to_string(),
        source: Box::new(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "the object already exists",
        )),
    }
}

fn precondition(path: &str) -> Error {
    Error::Precondition {
        path: path.to_string(),
        source: Box::new(std::io::Error::other("the ETag does not match")),
    }
}
