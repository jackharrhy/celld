use anyhow::{ensure, Context, Result};
use bytes::Bytes;
use celld_local_harness::Stores;
use futures_util::{FutureExt, TryStreamExt};
use object_store::{
    list::PaginatedListOptions, path::Path, GetOptions, GetRange, ObjectStore, PutMode,
    UpdateVersion,
};
use rand::{Rng, RngCore, SeedableRng};
use serde_json::{json, Value};
use std::{
    future::Future,
    panic::AssertUnwindSafe,
    sync::Arc,
    time::{Duration, Instant},
};

async fn case<F>(rows: &mut Vec<Value>, name: &str, f: F)
where
    F: Future<Output = Result<Value>>,
{
    let started = Instant::now();
    let result =
        tokio::time::timeout(Duration::from_secs(180), AssertUnwindSafe(f).catch_unwind()).await;
    let row = match result {
        Ok(Ok(Ok(details))) => json!({"test":name,"status":"pass","details":details}),
        Ok(Ok(Err(error))) => json!({"test":name,"status":"fail","error":format!("{error:#}")}),
        Ok(Err(panic)) => {
            let message = panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "non-string panic".into());
            json!({"test":name,"status":"fail","error":message})
        }
        Err(_) => json!({"test":name,"status":"fail","error":"180 second timeout"}),
    };
    let mut row = row;
    row["elapsed_ms"] = json!(started.elapsed().as_millis());
    eprintln!("{}: {} ({} ms)", name, row["status"], row["elapsed_ms"]);
    rows.push(row);
}

async fn race(
    store: Arc<dyn ObjectStore>,
    key: &Path,
    mode: PutMode,
    count: usize,
) -> Result<Value> {
    let barrier = Arc::new(tokio::sync::Barrier::new(count));
    let mut jobs = Vec::new();
    for i in 0..count {
        let store = store.clone();
        let barrier = barrier.clone();
        let key = key.clone();
        let mode = mode.clone();
        jobs.push(tokio::spawn(async move {
            barrier.wait().await;
            (
                i,
                store
                    .put_opts(&key, format!("claimant-{i}").into(), mode.into())
                    .await,
            )
        }));
    }
    let mut successes = Vec::new();
    let mut conflicts = 0;
    let mut errors = Vec::new();
    for job in jobs {
        let (i, result) = job.await?;
        match result {
            Ok(result) => successes.push((i, result)),
            Err(
                object_store::Error::AlreadyExists { .. }
                | object_store::Error::Precondition { .. },
            ) => conflicts += 1,
            Err(error) => errors.push(error.to_string()),
        }
    }
    ensure!(
        successes.len() == 1 && conflicts == count - 1 && errors.is_empty(),
        "{} successes, {conflicts} conflicts, errors={errors:?}",
        successes.len()
    );
    let (winner, result) = &successes[0];
    ensure!(
        result.e_tag.as_deref().is_some_and(|tag| !tag.is_empty()),
        "success did not include a nonempty ETag"
    );
    let get = store.get(key).await?;
    ensure!(
        get.meta.e_tag == result.e_tag,
        "read does not reflect winning generation"
    );
    ensure!(
        get.bytes().await?.as_ref() == format!("claimant-{winner}").as_bytes(),
        "winner body mismatch"
    );
    Ok(
        json!({"contenders":count,"successes":1,"conflicts":conflicts,"winner":winner,"etag":result.e_tag}),
    )
}

async fn conditional_races(stores: &Stores) -> Result<Value> {
    let store = &stores.objects;
    let key = Path::from("study/race");
    store.delete(&key).await.ok();
    let create = race(store.clone(), &key, PutMode::Create, 128).await?;
    let mut generations = Vec::new();
    for _ in 0..10 {
        let observed = store.head(&key).await?;
        let etag = observed.e_tag.context("ETag missing")?;
        let update = PutMode::Update(UpdateVersion {
            e_tag: Some(etag.clone()),
            version: observed.version,
        });
        let row = race(store.clone(), &key, update.clone(), 128).await?;
        ensure!(
            row["etag"] != json!(etag),
            "successful overwrite reused ETag"
        );
        ensure!(
            matches!(
                store.put_opts(&key, "stale".into(), update.into()).await,
                Err(object_store::Error::Precondition { .. })
            ),
            "old ETag accepted after winner committed"
        );
        generations.push(row);
    }
    let old = store.head(&key).await?;
    store.delete(&key).await?;
    let replacement = store.put(&key, "recreated".into()).await?;
    ensure!(
        replacement.e_tag != old.e_tag,
        "delete/recreate reused ETag (ABA)"
    );
    ensure!(
        matches!(
            store
                .put_opts(
                    &key,
                    "aba".into(),
                    PutMode::Update(UpdateVersion {
                        e_tag: old.e_tag,
                        version: old.version
                    })
                    .into()
                )
                .await,
            Err(object_store::Error::Precondition { .. })
        ),
        "ABA stale ETag accepted"
    );
    Ok(json!({"create":create,"cas_generations":generations,"delete_recreate_aba":"rejected"}))
}

async fn reads_and_ranges(stores: &Stores) -> Result<Value> {
    let store = &stores.objects;
    let mut rng = rand::rngs::StdRng::seed_from_u64(0xCE11D2026);
    let mut body = vec![0; 256 * 1024 + 73];
    rng.fill_bytes(&mut body);
    let key = Path::from("study/ranges");
    for generation in 0..64 {
        body[0..8].copy_from_slice(&(generation as u64).to_be_bytes());
        let put = store.put(&key, body.clone().into()).await?;
        let get = store.get(&key).await?;
        ensure!(get.meta.e_tag == put.e_tag, "stale immediate ETag");
        ensure!(get.bytes().await?.as_ref() == body, "stale immediate bytes");
    }
    for _ in 0..1024 {
        let start = rng.gen_range(0..body.len());
        let end = rng.gen_range(start + 1..=body.len());
        let get = store
            .get_opts(
                &key,
                GetOptions {
                    range: Some(GetRange::Bounded(start as u64..end as u64)),
                    ..Default::default()
                },
            )
            .await?;
        ensure!(
            get.range == (start as u64..end as u64),
            "range metadata mismatch"
        );
        ensure!(
            get.bytes().await?.as_ref() == &body[start..end],
            "range bytes mismatch"
        );
    }
    let beyond = store
        .get_range(&key, body.len() as u64 + 1..body.len() as u64 + 10)
        .await;
    ensure!(
        beyond.is_err() && !matches!(beyond, Err(object_store::Error::NotFound { .. })),
        "range starting beyond EOF succeeded or was misclassified as missing object"
    );
    Ok(
        json!({"seed":"0xCE11D2026","immediate_overwrites":64,"random_exact_ranges":1024,"beyond_eof":"error"}),
    )
}

async fn suffix_capability(stores: &Stores) -> Result<Value> {
    let key = Path::from("study/suffix");
    stores.objects.put(&key, "0123456789".into()).await?;
    match stores
        .objects
        .get_opts(
            &key,
            GetOptions {
                range: Some(GetRange::Suffix(4)),
                ..Default::default()
            },
        )
        .await
    {
        Ok(get) => {
            ensure!(
                get.bytes().await?.as_ref() == b"6789",
                "incorrect suffix bytes"
            );
            Ok(json!({"supported":true,"classification":"capability characterization"}))
        }
        Err(object_store::Error::NotSupported { source }) => Ok(
            json!({"supported":false,"error":source.to_string(),"classification":"capability characterization; exact bounded ranges tested separately"}),
        ),
        Err(e) => Err(e.into()),
    }
}

async fn escaped_key_roundtrip(stores: &Stores) -> Result<Value> {
    let key = Path::from("study/escaped/a%z/name");
    stores.objects.put(&key, "payload".into()).await?;
    let get = stores.objects.get(&key).await?;
    ensure!(
        get.meta.location == key,
        "GET metadata key changed: requested={key}, returned={}",
        get.meta.location
    );
    let listed = stores
        .objects
        .list(Some(&Path::from("study/escaped/a%z")))
        .try_collect::<Vec<_>>()
        .await?;
    ensure!(
        listed.len() == 1 && listed[0].location == key,
        "escaped prefix/list location failed: {listed:?}"
    );
    let expected_parent = Path::from("study/escaped/a%z");
    let grouped = stores
        .objects
        .list_with_delimiter(Some(&Path::from("study/escaped")))
        .await?;
    ensure!(
        grouped.common_prefixes == vec![expected_parent.clone()],
        "escaped delimiter prefix changed: {:?}",
        grouped.common_prefixes
    );
    let page = stores
        .listing
        .list_paginated(
            Some("study/escaped/"),
            PaginatedListOptions {
                max_keys: Some(1),
                delimiter: Some("/".into()),
                ..Default::default()
            },
        )
        .await?;
    ensure!(
        page.result.common_prefixes == vec![expected_parent],
        "escaped paginated delimiter changed: {:?}",
        page.result.common_prefixes
    );
    ensure!(
        stores
            .objects
            .get(&listed[0].location)
            .await?
            .bytes()
            .await?
            .as_ref()
            == b"payload",
        "listed key cannot be read back"
    );
    stores.objects.delete(&listed[0].location).await?;
    ensure!(
        matches!(
            stores.objects.get(&key).await,
            Err(object_store::Error::NotFound { .. })
        ),
        "delete by listed location left object"
    );
    Ok(json!({"key":key.to_string(),"get_metadata_list_get_delete":"pass"}))
}

async fn keys(stores: &Stores, prefix: &str) -> Result<Vec<String>> {
    Ok(stores
        .objects
        .list(Some(&Path::from(prefix)))
        .try_collect::<Vec<_>>()
        .await?
        .into_iter()
        .map(|m| m.location.to_string())
        .collect())
}

async fn listing(stores: &Stores, azure: bool) -> Result<Value> {
    let prefix = "study/list/";
    for i in (0..211).rev() {
        stores
            .objects
            .put(&Path::from(format!("{prefix}{i:04}")), "value".into())
            .await?;
    }
    stores
        .objects
        .put(&Path::from("study/list-other/x"), "excluded".into())
        .await?;
    let expected: Vec<_> = (0..211).map(|i| format!("{prefix}{i:04}")).collect();
    ensure!(
        keys(stores, "study/list").await? == expected,
        "recursive listing not ordered or wrong prefix"
    );
    let offset = Path::from(format!("{prefix}0100"));
    let after = stores
        .objects
        .list_with_offset(Some(&Path::from("study/list")), &offset)
        .try_collect::<Vec<_>>()
        .await?;
    ensure!(
        after
            .iter()
            .map(|m| m.location.to_string())
            .collect::<Vec<_>>()
            == expected[101..],
        "stream offset mismatch"
    );
    let mut token = None;
    let mut all = Vec::new();
    let mut pages = 0;
    loop {
        let page = stores
            .listing
            .list_paginated(
                Some(prefix),
                PaginatedListOptions {
                    max_keys: Some(7),
                    page_token: token.clone(),
                    ..Default::default()
                },
            )
            .await?;
        ensure!(page.result.objects.len() <= 7, "page exceeds max_keys");
        all.extend(
            page.result
                .objects
                .into_iter()
                .map(|m| m.location.to_string()),
        );
        pages += 1;
        if page.page_token.is_none() {
            break;
        }
        ensure!(page.page_token != token, "continuation stuck");
        token = page.page_token;
        ensure!(pages < 100, "pagination never terminated");
    }
    ensure!(
        all == expected,
        "pagination omitted, repeated, or reordered stable keys"
    );
    if !azure {
        let page = stores
            .listing
            .list_paginated(
                Some(prefix),
                PaginatedListOptions {
                    offset: Some(format!("{prefix}0100")),
                    max_keys: Some(3),
                    ..Default::default()
                },
            )
            .await?;
        ensure!(
            page.result
                .objects
                .iter()
                .map(|m| m.location.to_string())
                .collect::<Vec<_>>()
                == expected[101..104],
            "raw offset mismatch"
        );
    }
    // Deterministic concurrent-client mutations between two page requests.
    // A cursor is a position, not a snapshot: insertions behind it may be omitted.
    let p = "study/mutation/";
    for key in ["b", "d", "f", "h"] {
        stores
            .objects
            .put(&Path::from(format!("{p}{key}")), "value".into())
            .await?;
    }
    let first = stores
        .listing
        .list_paginated(
            Some(p),
            PaginatedListOptions {
                max_keys: Some(2),
                ..Default::default()
            },
        )
        .await?;
    let mutation = stores.objects.clone();
    tokio::spawn(async move {
        mutation.delete(&Path::from("study/mutation/d")).await?;
        mutation.delete(&Path::from("study/mutation/f")).await?;
        for name in ["a", "e", "z"] {
            mutation
                .put(&Path::from(format!("study/mutation/{name}")), "new".into())
                .await?;
        }
        Ok::<(), object_store::Error>(())
    })
    .await??;
    let second = stores
        .listing
        .list_paginated(
            Some(p),
            PaginatedListOptions {
                max_keys: Some(20),
                page_token: first.page_token.clone(),
                ..Default::default()
            },
        )
        .await?;
    let seen: Vec<_> = second
        .result
        .objects
        .iter()
        .map(|m| m.location.to_string())
        .collect();
    ensure!(
        seen == ["e", "h", "z"].map(|k| format!("{p}{k}")),
        "unexpected continuation after mutations: {seen:?}"
    );
    Ok(
        json!({"stable_keys":211,"pages":pages,"page_size":7,"stream_offset":"pass",
        "raw_offset":if azure {"unsupported by Azure; Celld rejects it"} else {"pass"},
        "mutation_next_page":seen,"snapshot_across_pages":false}),
    )
}

async fn delimiter_mutation(stores: &Stores) -> Result<Value> {
    let prefix = "study/delimiter/";
    for name in ["a/1", "b/1", "c/1"] {
        stores
            .objects
            .put(&Path::from(format!("{prefix}{name}")), "v".into())
            .await?;
    }
    let first = stores
        .listing
        .list_paginated(
            Some(prefix),
            PaginatedListOptions {
                delimiter: Some("/".into()),
                max_keys: Some(1),
                ..Default::default()
            },
        )
        .await?;
    let store = stores.objects.clone();
    tokio::spawn(async move {
        store
            .put(&Path::from("study/delimiter/a/9"), "new".into())
            .await
    })
    .await??;
    let second = stores
        .listing
        .list_paginated(
            Some(prefix),
            PaginatedListOptions {
                delimiter: Some("/".into()),
                max_keys: Some(1),
                page_token: first.page_token.clone(),
                ..Default::default()
            },
        )
        .await?;
    let old: Vec<_> = first
        .result
        .common_prefixes
        .iter()
        .map(ToString::to_string)
        .collect();
    let next: Vec<_> = second
        .result
        .common_prefixes
        .iter()
        .map(ToString::to_string)
        .collect();
    Ok(
        json!({"first":old,"next":next,"repeated_child_after_insertion":old==next,
        "classification":"characterization; no cross-page snapshot promised"}),
    )
}

async fn overlapping_listing(stores: &Stores) -> Result<Value> {
    let prefix = "study/churn/";
    let stable: Vec<_> = (0..32).map(|i| format!("{prefix}stable/{i:04}")).collect();
    for key in &stable {
        stores
            .objects
            .put(&Path::from(key.as_str()), "stable".into())
            .await?;
    }
    let writer = stores.objects.clone();
    let started = Arc::new(tokio::sync::Barrier::new(2));
    let other = started.clone();
    let task = tokio::spawn(async move {
        other.wait().await;
        for turn in 0..128 {
            let key = Path::from(format!("study/churn/volatile/{:04}", turn % 17));
            writer.put(&key, turn.to_string().into()).await?;
            if turn % 2 == 0 {
                writer.delete(&key).await?;
            }
            tokio::task::yield_now().await;
        }
        Ok::<_, object_store::Error>(())
    });
    started.wait().await;
    let mut pages = 0;
    for _ in 0..8 {
        let mut token = None;
        let mut seen = Vec::new();
        let mut this_walk = 0;
        loop {
            let page = stores
                .listing
                .list_paginated(
                    Some(prefix),
                    PaginatedListOptions {
                        max_keys: Some(5),
                        page_token: token.clone(),
                        ..Default::default()
                    },
                )
                .await?;
            ensure!(page.result.objects.len() <= 5, "churn page exceeded limit");
            seen.extend(
                page.result
                    .objects
                    .into_iter()
                    .map(|m| m.location.to_string()),
            );
            pages += 1;
            this_walk += 1;
            if page.page_token.is_none() {
                break;
            }
            ensure!(
                page.page_token != token && this_walk < 100,
                "churn pagination stuck"
            );
            token = page.page_token;
        }
        ensure!(
            seen.windows(2).all(|p| p[0] < p[1]),
            "churn walk repeated or reordered keys"
        );
        ensure!(
            stable.iter().all(|key| seen.contains(key)),
            "concurrent churn hid stable key"
        );
    }
    task.await??;
    Ok(
        json!({"concurrent_mutations":128,"stable_keys":32,"complete_walks":8,"pages":pages,
        "stable_keys_preserved":true,"duplicate_objects":0,"snapshot_across_pages":false}),
    )
}

async fn head_body(stores: &Stores) -> Result<Value> {
    let key = Path::from("study/head-body");
    stores.objects.put(&key, "nonempty".into()).await?;
    let get = stores
        .objects
        .get_opts(
            &key,
            GetOptions {
                head: true,
                ..Default::default()
            },
        )
        .await?;
    ensure!(get.meta.size == 8, "HEAD size metadata incorrect");
    let mut payload = get.into_stream();
    let mut returned = 0;
    while let Some(chunk) = payload.try_next().await? {
        returned += chunk.len();
    }
    ensure!(
        returned == 0,
        "GetOptions.head=true returned {returned} payload bytes; trait requests no content"
    );
    Ok(json!({"metadata_size":8,"payload_bytes":returned}))
}

async fn multipart_lifecycle(stores: &Stores) -> Result<Value> {
    let key = Path::from("study/multipart");
    stores.objects.put(&key, "old".into()).await?;
    let mut upload = stores.objects.put_multipart(&key).await?;
    upload.put_part(vec![7; 5 * 1024 * 1024].into()).await?;
    ensure!(
        stores.objects.get(&key).await?.bytes().await?.as_ref() == b"old",
        "uncommitted parts replaced target"
    );
    upload.abort().await?;
    ensure!(
        stores.objects.get(&key).await?.bytes().await?.as_ref() == b"old",
        "abort changed target"
    );
    let mut upload = stores.objects.put_multipart(&key).await?;
    let mut expected = vec![9; 5 * 1024 * 1024];
    let a = upload.put_part(expected.clone().into());
    let b = upload.put_part(Bytes::from_static(b"tail").into());
    b.await?;
    a.await?;
    upload.complete().await?;
    expected.extend_from_slice(b"tail");
    ensure!(
        stores.objects.get(&key).await?.bytes().await?.as_ref() == expected,
        "multipart part order or overwrite mismatch"
    );
    Ok(
        json!({"abort_preserves_old":true,"parts_hidden_until_complete":true,"overwrite_bytes":expected.len(),"out_of_order_polling":"pass"}),
    )
}

async fn ltx_adapter(stores: &Stores, azure: bool) -> Result<Value> {
    use celld_ltx::{
        client::{
            object_store::{ObjectStoreClient, ObjectStoreConfig, TimestampMetadataKey},
            ReplicaClient,
        },
        ltx, TXID,
    };
    let client = ObjectStoreClient::with_store(
        ObjectStoreConfig {
            path: "study/ltx".into(),
            timestamp_metadata_key: if azure {
                TimestampMetadataKey::Underscore
            } else {
                TimestampMetadataKey::Litestream
            },
            ..Default::default()
        },
        stores.objects.clone(),
    );
    let mut rng = rand::rngs::StdRng::seed_from_u64(1900);
    let mut lengths = Vec::new();
    let mut infos = Vec::new();
    for (txid, page_count) in [(1, 2), (2, 1600)] {
        let mut checksum = celld_ltx::CHECKSUM_FLAG;
        let mut pages = Vec::new();
        for page in 1..=page_count {
            let mut data = vec![0; 4096];
            rng.fill_bytes(&mut data);
            checksum ^= ltx::checksum_page(page, &data) & !celld_ltx::CHECKSUM_FLAG;
            pages.push((page, data));
        }
        let header = ltx::Header {
            version: ltx::VERSION,
            page_size: 4096,
            commit: page_count,
            min_txid: TXID(txid),
            max_txid: TXID(txid),
            timestamp: 1_788_600_000_000,
            pre_apply_checksum: if txid == 1 {
                0
            } else {
                celld_ltx::CHECKSUM_FLAG
            },
            ..Default::default()
        };
        let data = ltx::encode_file(&header, &pages, checksum)?;
        let info = client
            .write_ltx_file(0, TXID(txid), TXID(txid), &data)
            .await?;
        ensure!(
            client.open_ltx_file(0, TXID(txid), TXID(txid)).await? == data,
            "LTX adapter byte mismatch"
        );
        ensure!(
            client
                .read_range(0, TXID(txid), TXID(txid), 17, 123)
                .await?
                == data[17..140],
            "LTX range mismatch"
        );
        let reader = client.blocking_range_reader().await?;
        let copy = info.clone();
        let range =
            tokio::task::spawn_blocking(move || reader.read_range(&copy, 31, 257)).await??;
        ensure!(range == data[31..288], "paged blocking reader mismatch");
        lengths.push(data.len());
        infos.push(info);
    }
    ensure!(
        lengths[0] < 5 * 1024 * 1024 && lengths[1] >= 5 * 1024 * 1024,
        "fixture did not cross multipart threshold"
    );
    let listed = client.ltx_files_bounded(0, TXID(2), 1).await?;
    ensure!(
        listed.len() == 1 && listed[0].min_txid == TXID(2),
        "bounded LTX listing seek mismatch"
    );
    client.delete_ltx_files(&infos).await?;
    ensure!(
        !client.has_any_object().await?,
        "LTX batch delete left objects"
    );
    Ok(
        json!({"encoded_ltx_sizes":lengths,"single_put_and_multipart":true,"bounded_seek":true,"blocking_range_reader":true,"delete":true}),
    )
}

#[tokio::main(flavor = "multi_thread", worker_threads = 8)]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    ensure!(
        args.len() >= 4,
        "usage: conformance local DB REPORT | conformance azurite ENDPOINT CONTAINER REPORT"
    );
    let azure = args[1] == "azurite";
    let stores = if azure {
        celld_local_harness::azure(&args[2], &args[3])?
    } else {
        celld_local_harness::local(&args[2])?
    };
    let report = args.last().unwrap();
    let mut rows = Vec::new();
    use object_store::integration as upstream;
    macro_rules! upstream_case {($name:ident $(,$arg:expr)*)=>{
        case(&mut rows,concat!("object_store::integration::",stringify!($name)),async {
            // Helpers are independent tests. Some assume an empty store but
            // do not clear it themselves. Isolate local DBs so a bad escaped
            // listing cannot poison cleanup and create cascading failures.
            let scratch=tempfile::tempdir()?;
            let suite_stores=if azure {stores.clone()} else {celld_local_harness::local(scratch.path().join("objects.sqlite3"))?};
            if azure {
                let old=suite_stores.objects.list(None).try_collect::<Vec<_>>().await?;
                for object in old {suite_stores.objects.delete(&object.location).await?;}
            }
            let store=suite_stores.objects.as_ref();
            upstream::$name(store $(,$arg)*).await;Ok(json!({"source":"object_store 0.12.5 unmodified integration module"}))
        }).await;
    }}
    upstream_case!(put_get_delete_list);
    upstream_case!(put_get_attributes);
    upstream_case!(get_opts);
    upstream_case!(put_opts, true);
    upstream_case!(stream_get);
    upstream_case!(list_uses_directories_correctly);
    upstream_case!(list_with_delimiter);
    upstream_case!(rename_and_copy);
    upstream_case!(copy_if_not_exists);
    upstream_case!(copy_rename_nonexistent_object);
    upstream_case!(multipart_race_condition, !azure);
    upstream_case!(multipart_out_of_order);
    case(
        &mut rows,
        "object_store::integration::list_paginated",
        async {
            let scratch = tempfile::tempdir()?;
            let suite = if azure {
                stores.clone()
            } else {
                celld_local_harness::local(scratch.path().join("objects.sqlite3"))?
            };
            upstream::list_paginated(suite.objects.as_ref(), suite.listing.as_ref()).await;
            Ok(json!({"source":"object_store 0.12.5 unmodified integration module"}))
        },
    )
    .await;
    case(
        &mut rows,
        "conditional_create_and_cas_races",
        conditional_races(&stores),
    )
    .await;
    case(
        &mut rows,
        "read_after_write_and_exact_ranges",
        reads_and_ranges(&stores),
    )
    .await;
    case(
        &mut rows,
        "suffix_range_capability",
        suffix_capability(&stores),
    )
    .await;
    case(
        &mut rows,
        "lexical_listing_pagination_and_mutations",
        listing(&stores, azure),
    )
    .await;
    case(
        &mut rows,
        "delimiter_pagination_mutation_characterization",
        delimiter_mutation(&stores),
    )
    .await;
    case(
        &mut rows,
        "overlapping_mutations_and_paginated_walks",
        overlapping_listing(&stores),
    )
    .await;
    case(&mut rows, "head_has_no_payload", head_body(&stores)).await;
    case(
        &mut rows,
        "escaped_key_roundtrip",
        escaped_key_roundtrip(&stores),
    )
    .await;
    case(
        &mut rows,
        "multipart_complete_abort_overwrite",
        multipart_lifecycle(&stores),
    )
    .await;
    case(
        &mut rows,
        "actual_celld_ltx_adapter",
        ltx_adapter(&stores, azure),
    )
    .await;
    let failed = rows.iter().filter(|r| r["status"] == "fail").count();
    let result = json!({"backend":args[1],"upstream_base":"10cb1303dac710dcb3b557e318e08c855261f68b",
        "sqlite_version":rusqlite::version(),
        "local_store_source":env!("CELLD_TESTED_LOCAL_STORE"),
        "object_store":"0.12.5","tests":rows,"failed":failed,"passed":rows.len()-failed});
    std::fs::write(report, serde_json::to_string_pretty(&result)? + "\n")?;
    ensure!(failed == 0, "{failed} tests failed; see {report}");
    Ok(())
}
