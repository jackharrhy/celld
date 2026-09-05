use super::*;
use bytes::Bytes;
use object_store::ObjectStore;

async fn seed(store: &dyn ObjectStore, prefix: &str, azure_compatible: bool) -> Result<Vec<Path>> {
    let mut rich = Attributes::new();
    rich.insert(Attribute::ContentType, "audio/mpeg".into());
    rich.insert(Attribute::ContentLanguage, "en".into());
    rich.insert(
        Attribute::ContentDisposition,
        "inline; filename=sample.mp3".into(),
    );
    rich.insert(Attribute::CacheControl, "public, max-age=3600".into());
    let envelope = r#"{"custom":{"Title":"Radio sample","owner":"\u6771\u4eac"},"http":{"cacheExpiry":1893456000000},"checksums":{},"storageClass":"Standard"}"#;
    rich.insert(Attribute::Metadata("celld_r2".into()), envelope.into());
    let mut flat = Attributes::new();
    flat.insert(
        Attribute::Metadata("title".into()),
        "Legacy Radio sample".into(),
    );
    // An 8 MiB boundary crossing exercises bounded multipart transfer.
    let audio: Vec<u8> = (0..PART_BYTES + 101)
        .map(|index| (index.wrapping_mul(31) ^ (index >> 7)) as u8)
        .collect();
    let mut keys = Vec::new();
    let mut fixtures = vec![
        ("deploy/current.json", Bytes::from_static(br#"{"version":"same-version","prefix":"deploy/radio/same-version"}"#), Attributes::new()),
        ("nodes/radio.json", Bytes::from_static(br#"{"node":"radio","expires_ms":1,"log":{"state":"sealed","epoch":4,"ensemble":[],"tiered":17}}"#), Attributes::new()),
        ("cells/radio/own.json", Bytes::from_static(br#"{"node":"radio","epoch":7}"#), Attributes::new()),
        ("r2/audio/東京 100%/sample.mp3", Bytes::from(audio), rich),
        ("r2/audio/empty", Bytes::new(), Attributes::new()),
        ("r2/audio/legacy-flat", Bytes::from_static(b"legacy flat metadata"), flat),
        ("cells/radio/ltx/0000/0000000000000001-0000000000000001.ltx", Bytes::from_static(b"opaque LTX-shaped migration fixture; runtime LTX tests are separate"), Attributes::new()),
    ];
    if !azure_compatible {
        let mut legacy = Attributes::new();
        legacy.insert(Attribute::Metadata("celld-r2".into()), envelope.into());
        fixtures.push((
            "r2/audio/legacy-envelope",
            Bytes::from_static(b"legacy envelope"),
            legacy,
        ));
    }
    for (logical, body, attributes) in fixtures {
        let path = Path::from(format!("{prefix}{logical}"));
        if let Err(error) = store
            .put_opts(
                &path,
                body.into(),
                PutOptions {
                    mode: PutMode::Create,
                    attributes,
                    ..Default::default()
                },
            )
            .await
        {
            for created in &keys {
                store.delete(created).await?;
            }
            return Err(error.into());
        }
        keys.push(path);
    }
    Ok(keys)
}

fn opts(source: &str, destination: &str, manifest: &std::path::Path, resume: bool) -> Options {
    Options {
        mode: Mode::Copy,
        source: source.into(),
        destination: destination.into(),
        manifest: manifest.into(),
        resume,
    }
}

#[tokio::test]
async fn local_copy_resume_and_mismatch_refusal() -> Result<()> {
    // The production facade retains one process-wide Tokio domain. Keep the
    // default cases on one runtime instead of dropping its first runtime early.
    let temp = tempfile::tempdir()?;
    let source = format!("sqlite://{}", temp.path().join("source.sqlite3").display());
    let destination = format!(
        "sqlite://{}",
        temp.path().join("destination.sqlite3").display()
    );
    let manifest = temp.path().join("manifest.json");
    let source_bucket = Bucket::open(&source, None, "", None, None)?;
    seed(source_bucket.store.as_ref(), "", false).await?;
    let summary = run(opts(&source, &destination, &manifest, false)).await?;
    assert_eq!(summary.objects, 8);
    assert_eq!(summary.copied, 8);
    assert!(run(opts(&source, &destination, &manifest, false))
        .await
        .unwrap_err()
        .to_string()
        .contains("not empty"));
    let resumed = run(opts(&source, &destination, &manifest, true)).await?;
    assert_eq!(resumed.copied, 0);
    assert_eq!(resumed.already_verified, 8);

    let destination_bucket = Bucket::open(&destination, None, "", None, None)?;
    destination_bucket
        .store
        .delete(&Path::from("r2/audio/empty"))
        .await?;
    assert_eq!(
        run(opts(&source, &destination, &manifest, true))
            .await?
            .copied,
        1
    );
    destination_bucket
        .store
        .put(
            &Path::from("unexpected"),
            Bytes::from_static(b"retain me").into(),
        )
        .await?;
    assert!(run(opts(&source, &destination, &manifest, true))
        .await
        .unwrap_err()
        .to_string()
        .contains("unexpected destination key"));
    destination_bucket
        .store
        .delete(&Path::from("unexpected"))
        .await?;
    destination_bucket
        .store
        .put(
            &Path::from("deploy/current.json"),
            Bytes::from_static(b"different").into(),
        )
        .await?;
    assert!(run(opts(&source, &destination, &manifest, true))
        .await
        .unwrap_err()
        .to_string()
        .contains("content mismatch"));
    assert_eq!(
        destination_bucket
            .store
            .get(&Path::from("deploy/current.json"))
            .await?
            .bytes()
            .await?,
        "different"
    );
    source_drift_and_active_runtime_are_refused().await
}

async fn source_drift_and_active_runtime_are_refused() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let source_path = temp.path().join("source.sqlite3");
    let source = format!("sqlite://{}", source_path.display());
    let destination = format!(
        "sqlite://{}",
        temp.path().join("destination.sqlite3").display()
    );
    let manifest = temp.path().join("manifest.json");
    let source_bucket = Bucket::open(&source, None, "", None, None)?;
    source_bucket
        .store
        .put(&Path::from("one"), Bytes::from_static(b"before").into())
        .await?;
    let guard = celld::local_storage::lock_runtime(&source_path)?;
    assert!(run(opts(&source, &destination, &manifest, false))
        .await
        .is_err());
    drop(guard);
    run(opts(&source, &destination, &manifest, false)).await?;
    source_bucket
        .store
        .put(&Path::from("one"), Bytes::from_static(b"after").into())
        .await?;
    assert!(run(opts(&source, &destination, &manifest, true))
        .await
        .is_err());
    Ok(())
}

/// Creates and deletes fixtures only in an explicitly supplied empty test
/// namespace. The Azure container must already exist.
#[tokio::test]
#[ignore = "requires an isolated Azurite container; see docs/store-copy.md"]
async fn azure_local_azure_roundtrip() -> Result<()> {
    let source = std::env::var("CELLD_STORE_COPY_TEST_AZURE")
        .context("set CELLD_STORE_COPY_TEST_AZURE=az://celld-copy-test-NAME/source")?;
    ensure!(
        source.starts_with("az://celld-copy-test-") && source.ends_with("/source"),
        "rehearsal requires az://celld-copy-test-NAME/source"
    );
    let return_spec = format!("{}/returned", source.trim_end_matches("/source"));
    let source_bucket = Bucket::open(&source, None, "", None, None)?;
    let return_bucket = Bucket::open(&return_spec, None, "", None, None)?;
    ensure!(
        inventory(&source_bucket).await?.is_empty() && inventory(&return_bucket).await?.is_empty(),
        "rehearsal namespaces must be empty"
    );
    let source_keys = seed(source_bucket.store.as_ref(), &source_bucket.prefix, true).await?;
    let temp = tempfile::tempdir()?;
    let local = format!("sqlite://{}", temp.path().join("objects.sqlite3").display());
    let rehearsal: Result<()> = async {
        let forward_manifest = temp.path().join("forward.json");
        let forward = run(opts(&source, &local, &forward_manifest, false)).await?;
        assert_eq!(forward.copied, 7);
        let resume = run(opts(&source, &local, &forward_manifest, true)).await?;
        assert_eq!(resume.copied, 0);
        let mut verification = opts(&source, &local, &forward_manifest, false);
        verification.mode = Mode::Verify;
        run(verification).await?;
        let reverse = run(opts(&local, &return_spec, &temp.path().join("reverse.json"), false)).await?;
        assert_eq!(reverse.copied, 7);
        eprintln!("Azurite -> SQLite -> Azurite: seven objects, opaque roots, current envelope and legacy metadata, percent/unicode keys, empty blob and multipart audio verified");
        Ok(())
    }.await;
    for path in source_keys {
        source_bucket.store.delete(&path).await?;
    }
    for logical in inventory(&return_bucket).await?.keys() {
        return_bucket
            .store
            .delete(&key(&return_bucket, logical)?)
            .await?;
    }
    rehearsal
}
