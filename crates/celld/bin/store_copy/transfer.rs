use anyhow::{bail, ensure, Context, Result};
use bytes::BytesMut;
use celld::bucket::Bucket;
use futures_util::StreamExt;
use object_store::{
    path::Path, Attribute, Attributes, GetOptions, ObjectMeta, PutMode, PutMultipartOptions,
    PutOptions,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{BufReader, Write},
    path::PathBuf,
};

const PART_BYTES: usize = 8 * 1024 * 1024;
const MANIFEST_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    Copy,
    Verify,
}

pub(super) struct Options {
    pub mode: Mode,
    pub source: String,
    pub destination: String,
    pub manifest: PathBuf,
    pub resume: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
struct StoredAttribute {
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    value: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct ObjectRecord {
    key: String,
    size: u64,
    sha256: String,
    attributes: Vec<StoredAttribute>,
    source_etag: Option<String>,
    source_version: Option<String>,
    source_modified_ms: i64,
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    format_version: u32,
    source: String,
    destination: String,
    objects: Vec<ObjectRecord>,
}

#[derive(Debug, Serialize)]
pub(super) struct Summary {
    status: &'static str,
    source: String,
    destination: String,
    manifest: PathBuf,
    objects: usize,
    bytes: u64,
    copied: usize,
    already_verified: usize,
    destination_etags_and_modified_times_regenerated: bool,
}

#[derive(Serialize)]
pub(super) struct RootAudit {
    source: String,
    pub folded_roots_sealed_or_absent: bool,
    scope: &'static str,
    node_roots: Vec<serde_json::Value>,
    unsealed_or_malformed_keys: Vec<String>,
}

pub(super) async fn audit(spec: &str) -> Result<RootAudit> {
    let (bucket, _guard) = open(spec, true)?;
    audit_bucket(&bucket, spec).await
}

async fn audit_bucket(bucket: &Bucket, spec: &str) -> Result<RootAudit> {
    let mut report = RootAudit {
        source: spec.to_string(), folded_roots_sealed_or_absent: true,
        scope: "current-version nodes/*.json folded roots; does not prove retained application ACKs or inspect runtime disks",
        node_roots: Vec::new(), unsealed_or_malformed_keys: Vec::new(),
    };
    for (logical, meta) in inventory(bucket).await? {
        if !logical.starts_with("nodes/") || !logical.ends_with(".json") {
            continue;
        }
        ensure!(
            meta.size <= 1024 * 1024,
            "unexpectedly large node record {logical:?}"
        );
        let bytes = bucket
            .store
            .get(&key(bucket, &logical)?)
            .await?
            .bytes()
            .await?;
        let wire: Result<serde_json::Value, _> = serde_json::from_slice(&bytes);
        let (state, node, expiry, epoch, tiered) = match &wire {
            Ok(wire)
                if wire.get("node").is_some_and(serde_json::Value::is_string)
                    && wire
                        .get("expires_ms")
                        .and_then(serde_json::Value::as_u64)
                        .is_some() =>
            {
                let log = wire.get("log").filter(|value| !value.is_null());
                let state = match log {
                    None => "absent",
                    Some(log)
                        if log
                            .get("epoch")
                            .and_then(serde_json::Value::as_u64)
                            .is_some()
                            && log
                                .get("tiered")
                                .and_then(serde_json::Value::as_u64)
                                .is_some()
                            && log.get("ensemble").is_some_and(serde_json::Value::is_array) =>
                    {
                        log.get("state")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("malformed")
                    }
                    Some(_) => "malformed",
                };
                (
                    state,
                    wire.get("node").cloned(),
                    wire.get("expires_ms").cloned(),
                    log.and_then(|value| value.get("epoch")).cloned(),
                    log.and_then(|value| value.get("tiered")).cloned(),
                )
            }
            _ => ("malformed", None, None, None, None),
        };
        if !matches!(state, "sealed" | "absent") {
            report.unsealed_or_malformed_keys.push(logical.clone());
        }
        report
            .node_roots
            .push(serde_json::json!({"key":logical,"node":node,
            "log_state":state,"lease_expires_ms":expiry,"log_epoch":epoch,"tiered":tiered}));
    }
    report.folded_roots_sealed_or_absent = report.unsealed_or_malformed_keys.is_empty();
    Ok(report)
}

fn attributes(attributes: &Attributes) -> Result<Vec<StoredAttribute>> {
    let mut stored = Vec::new();
    for (attribute, value) in attributes {
        let (kind, name) = match attribute {
            Attribute::ContentDisposition => ("content-disposition", None),
            Attribute::ContentEncoding => ("content-encoding", None),
            Attribute::ContentLanguage => ("content-language", None),
            Attribute::ContentType => ("content-type", None),
            Attribute::CacheControl => ("cache-control", None),
            Attribute::StorageClass => ("storage-class", None),
            Attribute::Metadata(name) => ("metadata", Some(name.to_string())),
            other => bail!("unsupported object attribute {other:?}; refusing lossy copy"),
        };
        stored.push(StoredAttribute {
            kind: kind.into(),
            name,
            value: value.as_ref().into(),
        });
    }
    stored.sort();
    Ok(stored)
}

fn key(bucket: &Bucket, logical: &str) -> Result<Path> {
    // Listed keys are already the canonical object_store path. Path::from
    // would percent-encode them a second time and silently move R2 objects.
    Ok(Path::parse(format!("{}{logical}", bucket.prefix))?)
}

async fn inventory(bucket: &Bucket) -> Result<BTreeMap<String, ObjectMeta>> {
    let prefix = Path::parse(bucket.prefix.trim_end_matches('/'))?;
    let mut stream = bucket.store.list(Some(&prefix));
    let mut objects = BTreeMap::new();
    while let Some(meta) = stream.next().await {
        let meta = meta?;
        let logical = meta
            .location
            .as_ref()
            .strip_prefix(&bucket.prefix)
            .context("store listed an object outside the requested namespace")?
            .to_string();
        ensure!(
            !logical.is_empty(),
            "namespace contains an object with an empty logical key"
        );
        ensure!(
            objects.insert(logical.clone(), meta).is_none(),
            "duplicate listed key {logical:?}"
        );
    }
    Ok(objects)
}

async fn fingerprint(bucket: &Bucket, logical: &str) -> Result<ObjectRecord> {
    let result = bucket.store.get(&key(bucket, logical)?).await?;
    let mut record = ObjectRecord {
        key: logical.to_string(),
        size: result.meta.size,
        sha256: String::new(),
        attributes: attributes(&result.attributes)?,
        source_etag: result.meta.e_tag.clone(),
        source_version: result.meta.version.clone(),
        source_modified_ms: result.meta.last_modified.timestamp_millis(),
    };
    let mut hash = Sha256::new();
    let mut count = 0_u64;
    let mut body = result.into_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        count += chunk.len() as u64;
        hash.update(&chunk);
    }
    ensure!(count == record.size, "size mismatch reading {logical:?}");
    record.sha256 = format!("{:x}", hash.finalize());
    Ok(record)
}

fn same_content(expected: &ObjectRecord, actual: &ObjectRecord) -> Result<()> {
    ensure!(
        expected.key == actual.key
            && expected.size == actual.size
            && expected.sha256 == actual.sha256,
        "content mismatch for {:?}; destination is never overwritten",
        expected.key
    );
    ensure!(
        expected.attributes == actual.attributes,
        "attribute mismatch for {:?}; destination is never overwritten",
        expected.key
    );
    Ok(())
}

async fn verify_namespace(bucket: &Bucket, records: &[ObjectRecord], source: bool) -> Result<()> {
    let listed = inventory(bucket).await?;
    ensure!(
        listed.len() == records.len()
            && records
                .iter()
                .all(|record| listed.contains_key(&record.key)),
        "{} keyset differs from manifest",
        if source { "source" } else { "destination" }
    );
    for record in records {
        let actual = fingerprint(bucket, &record.key).await?;
        same_content(record, &actual)?;
        if source {
            ensure!(
                *record == actual,
                "source generation changed for {:?}; re-establish a quiescent snapshot",
                record.key
            );
        }
    }
    let after = inventory(bucket).await?;
    ensure!(
        listed.keys().eq(after.keys()),
        "namespace keyset changed during verification"
    );
    for (logical, before) in listed {
        let now = &after[&logical];
        ensure!(
            before.e_tag == now.e_tag
                && before.version == now.version
                && before.size == now.size
                && before.last_modified == now.last_modified,
            "object changed during verification: {logical:?}"
        );
    }
    Ok(())
}

async fn copy_object(source: &Bucket, destination: &Bucket, record: &ObjectRecord) -> Result<()> {
    let result = source
        .store
        .get_opts(
            &key(source, &record.key)?,
            GetOptions {
                if_match: record.source_etag.clone(),
                ..Default::default()
            },
        )
        .await?;
    ensure!(
        result.meta.size == record.size && attributes(&result.attributes)? == record.attributes,
        "source changed before copying {:?}",
        record.key
    );
    let destination_key = key(destination, &record.key)?;
    let object_attributes = result.attributes.clone();
    let mut stream = result.into_stream();
    let mut hash = Sha256::new();
    let mut count = 0_u64;
    if record.size <= PART_BYTES as u64 {
        let mut body = BytesMut::with_capacity(record.size as usize);
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            count += chunk.len() as u64;
            ensure!(
                count <= record.size,
                "source exceeded expected size for {:?}",
                record.key
            );
            hash.update(&chunk);
            body.extend_from_slice(&chunk);
        }
        ensure!(
            count == record.size && format!("{:x}", hash.finalize()) == record.sha256,
            "source content changed for {:?}",
            record.key
        );
        destination
            .cas_store
            .put_opts(
                &destination_key,
                body.freeze().into(),
                PutOptions {
                    mode: PutMode::Create,
                    attributes: object_attributes,
                    ..Default::default()
                },
            )
            .await?;
    } else {
        ensure!(
            destination
                .store
                .head(&destination_key)
                .await
                .is_err_and(|error| matches!(error, object_store::Error::NotFound { .. })),
            "destination key exists or could not be checked: {:?}",
            record.key
        );
        let mut upload = destination
            .store
            .put_multipart_opts(
                &destination_key,
                PutMultipartOptions {
                    attributes: object_attributes,
                    ..Default::default()
                },
            )
            .await?;
        let transferred: Result<()> = async {
            let mut pending = BytesMut::with_capacity(PART_BYTES);
            while let Some(chunk) = stream.next().await {
                let mut chunk = chunk?;
                count += chunk.len() as u64;
                ensure!(
                    count <= record.size,
                    "source exceeded expected size for {:?}",
                    record.key
                );
                hash.update(&chunk);
                while !chunk.is_empty() {
                    let take = chunk.len().min(PART_BYTES - pending.len());
                    pending.extend_from_slice(&chunk.split_to(take));
                    if pending.len() == PART_BYTES {
                        upload.put_part(pending.split().freeze().into()).await?;
                    }
                }
            }
            ensure!(
                count == record.size && format!("{:x}", hash.finalize()) == record.sha256,
                "source content changed for {:?}",
                record.key
            );
            if !pending.is_empty() {
                upload.put_part(pending.freeze().into()).await?;
            }
            upload.complete().await?;
            Ok(())
        }
        .await;
        if let Err(error) = transferred {
            let _ = upload.abort().await;
            return Err(error);
        }
    }
    same_content(record, &fingerprint(destination, &record.key).await?)
}

fn open(spec: &str, source: bool) -> Result<(Bucket, Option<File>)> {
    let local = celld::local_storage::path_from_spec(spec)?;
    ensure!(
        local.is_some() || spec.starts_with("az://"),
        "only az:// and sqlite:// stores are supported"
    );
    if source {
        if let Some(path) = &local {
            ensure!(
                path.is_file(),
                "source SQLite database does not exist: {}",
                path.display()
            );
        }
    }
    let guard = local
        .as_deref()
        .map(celld::local_storage::lock_runtime)
        .transpose()?;
    Ok((
        Bucket::open(spec, None, "us-east-1", None, Some("store-copy"))?,
        guard,
    ))
}

pub(super) async fn run(options: Options) -> Result<Summary> {
    ensure!(
        options.source != options.destination,
        "source and destination must differ"
    );
    let (source, _source_guard) = open(&options.source, true)?;
    let (destination, _destination_guard) =
        open(&options.destination, options.mode == Mode::Verify)?;
    ensure!(
        source.backend != destination.backend
            || source.name != destination.name
            || !(source.prefix.starts_with(&destination.prefix)
                || destination.prefix.starts_with(&source.prefix)),
        "source and destination namespaces overlap"
    );
    let existing = inventory(&destination).await?;
    if options.mode == Mode::Copy && !options.resume {
        ensure!(
            existing.is_empty(),
            "destination is not empty; use --resume only with this copy's existing manifest"
        );
        ensure!(
            !options.manifest.exists(),
            "manifest already exists; use --resume or choose a fresh manifest"
        );
    }
    let manifest = if options.mode == Mode::Verify || options.resume {
        let manifest: Manifest = serde_json::from_reader(BufReader::new(
            File::open(&options.manifest).context("open existing manifest")?,
        ))?;
        ensure!(
            manifest.format_version == MANIFEST_VERSION,
            "unsupported manifest format"
        );
        ensure!(
            manifest.source == options.source && manifest.destination == options.destination,
            "manifest belongs to different source/destination specifications"
        );
        verify_namespace(&source, &manifest.objects, true).await?;
        manifest
    } else {
        let listed = inventory(&source).await?;
        ensure!(
            !listed.is_empty(),
            "source namespace is empty; refusing a likely wrong source"
        );
        let mut objects = Vec::with_capacity(listed.len());
        for logical in listed.keys() {
            objects.push(fingerprint(&source, logical).await?);
        }
        verify_namespace(&source, &objects, true).await?;
        let manifest = Manifest {
            format_version: MANIFEST_VERSION,
            source: options.source.clone(),
            destination: options.destination.clone(),
            objects,
        };
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&options.manifest)
            .context("create manifest before any destination writes")?;
        serde_json::to_writer_pretty(&mut file, &manifest)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        let parent = options
            .manifest
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        File::open(parent)?.sync_all()?;
        manifest
    };
    let by_key: BTreeMap<_, _> = manifest
        .objects
        .iter()
        .map(|record| (record.key.as_str(), record))
        .collect();
    ensure!(
        by_key.len() == manifest.objects.len(),
        "duplicate manifest keys"
    );
    for logical in existing.keys() {
        let expected = by_key
            .get(logical.as_str())
            .with_context(|| format!("unexpected destination key {logical:?}"))?;
        same_content(expected, &fingerprint(&destination, logical).await?)?;
    }
    let mut copied = 0;
    if options.mode == Mode::Copy {
        for record in &manifest.objects {
            if !existing.contains_key(&record.key) {
                celld::note!("copy {:?} ({} bytes)", record.key, record.size);
                copy_object(&source, &destination, record).await?;
                copied += 1;
            }
        }
    }
    verify_namespace(&source, &manifest.objects, true).await?;
    verify_namespace(&destination, &manifest.objects, false).await?;
    Ok(Summary {
        status: "verified",
        source: options.source,
        destination: options.destination,
        manifest: options.manifest,
        objects: manifest.objects.len(),
        bytes: manifest.objects.iter().map(|r| r.size).sum(),
        copied,
        already_verified: existing.len(),
        destination_etags_and_modified_times_regenerated: true,
    })
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
