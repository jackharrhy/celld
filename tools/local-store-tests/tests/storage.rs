use bytes::Bytes;
use celld_local_harness::local;
use futures_util::{StreamExt as _, TryStreamExt as _};
use object_store::{list::PaginatedListOptions, path::Path, Error, GetOptions, GetRange};

#[tokio::test]
async fn incomplete_and_aborted_uploads_never_replace_the_visible_object() {
    let dir = tempfile::tempdir().unwrap();
    let stores = local(dir.path().join("objects.sqlite3")).unwrap();
    let key = Path::from("media");
    let original = stores.objects.put(&key, "original".into()).await.unwrap();
    let mut upload = stores.objects.put_multipart(&key).await.unwrap();
    let delayed = upload.put_part("first".into());
    upload.put_part("second".into()).await.unwrap();
    assert!(upload.complete().await.is_err());
    assert_eq!(
        stores.objects.head(&key).await.unwrap().e_tag,
        original.e_tag
    );
    delayed.await.unwrap();
    upload.abort().await.unwrap();
    assert!(upload.complete().await.is_err());
    assert_eq!(
        stores
            .objects
            .get(&key)
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap(),
        "original"
    );

    let mut upload = stores.objects.put_multipart(&key).await.unwrap();
    let delayed = upload.put_part("cannot resurrect".into());
    upload.abort().await.unwrap();
    assert!(delayed.await.is_err());
    assert_eq!(
        stores.objects.head(&key).await.unwrap().e_tag,
        original.e_tag
    );
}

#[tokio::test]
async fn snapshot_and_shared_copy_survive_overwrite_delete_and_complete_reclamation() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("objects.sqlite3");
    let stores = local(&db).unwrap();
    let key = Path::from("old");
    let copy = Path::from("copy");
    let bytes = Bytes::from(vec![0x5a; 19 * 1024 * 1024 + 17]);
    stores
        .objects
        .put(&key, bytes.clone().into())
        .await
        .unwrap();
    stores.objects.copy(&key, &copy).await.unwrap();
    let old_reader = stores.objects.get(&key).await.unwrap();
    let ranged_reader = stores
        .objects
        .get_opts(
            &key,
            GetOptions {
                range: Some(GetRange::Bounded(1024 * 1024 - 3..1024 * 1024 + 11)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    stores.objects.put(&key, "new".into()).await.unwrap();
    assert_eq!(
        stores.objects.head(&copy).await.unwrap().size,
        bytes.len() as u64
    );
    stores.objects.delete(&copy).await.unwrap();
    assert!(matches!(
        stores.objects.head(&copy).await,
        Err(Error::NotFound { .. })
    ));
    // Foreground reclamation is bounded to two chunks per successful write.
    let connection = rusqlite::Connection::open(&db).unwrap();
    let remaining = connection
        .query_row("SELECT count(*) FROM local_chunks", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap();
    assert_eq!(remaining, 18);
    for i in 0..10 {
        stores
            .objects
            .put(&Path::from(format!("later/{i}")), "metadata".into())
            .await
            .unwrap();
    }
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM local_chunks", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(ranged_reader.bytes().await.unwrap(), bytes.slice(0..14));
    let mut total = 0;
    let mut stream = old_reader.into_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap();
        assert!(chunk.len() <= 1024 * 1024);
        assert!(chunk.iter().all(|b| *b == 0x5a));
        total += chunk.len();
    }
    assert_eq!(total, bytes.len());
    assert_eq!(
        stores
            .objects
            .get(&key)
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap(),
        "new"
    );
}

#[tokio::test]
async fn legacy_inline_database_migrates_without_changing_key_etag_or_attributes() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("objects.sqlite3");
    let key = Path::from("escaped/a%z");
    let original = vec![0x7b; 3 * 1024 * 1024 + 13];
    let connection = rusqlite::Connection::open(&db).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE objects(key TEXT PRIMARY KEY,body BLOB NOT NULL,
        etag INTEGER NOT NULL,modified_ms INTEGER NOT NULL,attributes TEXT NOT NULL);
        CREATE TABLE store_sequence(singleton INTEGER PRIMARY KEY,next_etag INTEGER NOT NULL);
        INSERT INTO store_sequence VALUES(1,43);",
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO objects VALUES(?1,?2,42,1000,?3)",
            rusqlite::params![
                key.as_ref(),
                &original,
                r#"[{"kind":"content-type","name":null,"value":"audio/test"}]"#
            ],
        )
        .unwrap();
    drop(connection);
    let stores = local(&db).unwrap();
    let head = stores.objects.head(&key).await.unwrap();
    assert_eq!(head.location, key);
    assert_eq!(head.e_tag.as_deref(), Some("42"));
    assert_eq!(head.size, original.len() as u64);
    let ranged = stores
        .objects
        .get_opts(
            &key,
            GetOptions {
                range: Some(GetRange::Bounded(3 * 1024 * 1024..original.len() as u64)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        ranged
            .attributes
            .get(&object_store::Attribute::ContentType)
            .unwrap()
            .as_ref(),
        "audio/test"
    );
    assert_eq!(
        ranged.bytes().await.unwrap().as_ref(),
        &original[3 * 1024 * 1024..]
    );
    let mut stream = stores.objects.get(&key).await.unwrap().into_stream();
    let mut count = 0;
    while let Some(bytes) = stream.try_next().await.unwrap() {
        assert!(bytes.len() <= 1024 * 1024);
        count += bytes.len();
    }
    assert_eq!(count, original.len());
    let replacement = stores.objects.put(&key, "next".into()).await.unwrap();
    assert_eq!(replacement.e_tag.as_deref(), Some("43"));
}

#[tokio::test]
async fn expired_upload_is_fenced_and_reclaimed_but_active_upload_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("objects.sqlite3");
    let stores = local(&db).unwrap();
    let key = Path::from("staging");
    let mut upload = stores.objects.put_multipart(&key).await.unwrap();
    upload
        .put_part(Bytes::from(vec![9; 2 * 1024 * 1024]).into())
        .await
        .unwrap();
    let reopened = local(&db).unwrap();
    assert!(matches!(
        reopened.objects.head(&key).await,
        Err(Error::NotFound { .. })
    ));
    let connection = rusqlite::Connection::open(&db).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM local_chunks", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
    connection
        .execute("UPDATE local_uploads SET touched_ms=0 WHERE state=0", [])
        .unwrap();
    let _reopened = local(&db).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM local_chunks", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert!(upload.put_part("late".into()).await.is_err());
    assert!(upload.complete().await.is_err());
}

#[tokio::test]
async fn late_pages_and_large_common_prefix_keep_exact_order_and_key_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("objects.sqlite3");
    let stores = local(&db).unwrap();
    let mut connection = rusqlite::Connection::open(&db).unwrap();
    let tx = connection.transaction().unwrap();
    for i in 0..10_000 {
        tx.execute("INSERT INTO objects(key,body,etag,modified_ms,attributes,size) VALUES(?1,X'',1,0,'[]',0)",
            [format!("many/group/{i:05}")]).unwrap();
    }
    for key in [
        "many/z",
        "many0/other",
        "many/é/one",
        "many/é/two",
        "many/ê",
    ] {
        tx.execute("INSERT INTO objects(key,body,etag,modified_ms,attributes,size) VALUES(?1,X'',1,0,'[]',0)", [key]).unwrap();
    }
    tx.commit().unwrap();
    let first = stores
        .listing
        .list_paginated(
            Some("many/"),
            PaginatedListOptions {
                delimiter: Some("/".into()),
                max_keys: Some(1),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(first.result.common_prefixes, vec![Path::from("many/group")]);
    let second = stores
        .listing
        .list_paginated(
            Some("many/"),
            PaginatedListOptions {
                delimiter: Some("/".into()),
                max_keys: Some(2),
                page_token: first.page_token,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        second
            .result
            .objects
            .iter()
            .map(|m| m.location.as_ref())
            .collect::<Vec<_>>(),
        ["many/z"]
    );
    assert_eq!(
        second.result.common_prefixes,
        vec![Path::parse("many/é").unwrap()]
    );
    let last = stores
        .listing
        .list_paginated(
            Some("many/"),
            PaginatedListOptions {
                delimiter: Some("/".into()),
                max_keys: Some(2),
                page_token: second.page_token,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        last.result
            .objects
            .iter()
            .map(|m| m.location.as_ref())
            .collect::<Vec<_>>(),
        ["many/ê"]
    );
    assert!(last.page_token.is_none());
    let page = stores
        .listing
        .list_paginated(
            Some("many/group/"),
            PaginatedListOptions {
                offset: Some("many/group/09996".into()),
                max_keys: Some(2),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        page.result
            .objects
            .iter()
            .map(|m| m.location.as_ref())
            .collect::<Vec<_>>(),
        ["many/group/09997", "many/group/09998"]
    );
}

#[test]
fn create_missing_parent_and_reject_future_schema_without_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("new/nested/objects.sqlite3");
    assert!(local(&db).is_ok());
    let connection = rusqlite::Connection::open(&db).unwrap();
    connection.pragma_update(None, "user_version", 999).unwrap();
    assert!(local(&db).is_err());
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        999
    );
}
