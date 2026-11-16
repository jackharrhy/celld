//! Opt-in one-GiB qualification. Run under /usr/bin/time -v or a memory cgroup.
use anyhow::{ensure, Result};
use bytes::Bytes;
use futures_util::TryStreamExt as _;
use object_store::{path::Path, GetOptions, GetRange};

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let db = std::env::args().nth(1).expect("usage: large-object DB");
    let stores = celld_local_harness::local(&db)?;
    let key = Path::from("qualification/one-gib");
    const PART: usize = 8 * 1024 * 1024;
    const PARTS: usize = 128;
    const SIZE: usize = PART * PARTS;
    let start = std::time::Instant::now();
    let mut upload = stores.objects.put_multipart(&key).await?;
    for part in 0..PARTS {
        upload
            .put_part(Bytes::from(vec![part as u8; PART]).into())
            .await?;
        if (part + 1) % 32 == 0 {
            eprintln!("staged {} MiB", (part + 1) * 8);
        }
    }
    let published = upload.complete().await?;
    ensure!(stores.objects.head(&key).await?.size == SIZE as u64);
    let head = stores
        .objects
        .get_opts(
            &key,
            GetOptions {
                head: true,
                ..Default::default()
            },
        )
        .await?;
    ensure!(head.bytes().await?.is_empty());
    for range in [
        0..17,
        (PART - 3) as u64..(PART + 11) as u64,
        (SIZE - 17) as u64..SIZE as u64,
    ] {
        let data = stores.objects.get_range(&key, range.clone()).await?;
        ensure!(data.len() as u64 == range.end - range.start);
        for (index, byte) in data.iter().enumerate() {
            ensure!(*byte == ((range.start as usize + index) / PART) as u8);
        }
    }
    let suffix = stores
        .objects
        .get_opts(
            &key,
            GetOptions {
                range: Some(GetRange::Suffix(37)),
                ..Default::default()
            },
        )
        .await?
        .bytes()
        .await?;
    ensure!(suffix.len() == 37 && suffix.iter().all(|b| *b == 127));
    let mut body = stores.objects.get(&key).await?.into_stream();
    let mut position = 0usize;
    while let Some(bytes) = body.try_next().await? {
        ensure!(bytes.len() <= 1024 * 1024);
        for (index, byte) in bytes.iter().enumerate() {
            ensure!(*byte == ((position + index) / PART) as u8);
        }
        position += bytes.len();
    }
    ensure!(position == SIZE);
    // Reopen and check the last byte as well as metadata; this must be a durable
    // object larger than SQLite's default maximum individual BLOB length.
    let reopened = celld_local_harness::local(&db)?;
    ensure!(reopened.objects.head(&key).await?.e_tag == published.e_tag);
    ensure!(
        reopened
            .objects
            .get_range(&key, SIZE as u64 - 1..SIZE as u64)
            .await?[0]
            == 127
    );
    println!(
        "{}",
        serde_json::json!({"status":"pass", "bytes":SIZE,
        "sqlite_version":rusqlite::version(), "elapsed_ms":start.elapsed().as_millis(),
        "multipart_part_bytes":PART, "read_chunk_max_bytes":1024*1024,
        "checks":["multipart_1GiB","HEAD","bounded_ranges","suffix","streamed_full_read","reopen"]})
    );
    Ok(())
}
