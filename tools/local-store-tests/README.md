# Local store qualification

This small Rust package compiles the fork's actual `local_store.rs` and LTX
adapter, supplying only the runtime blocking/wall-clock adapter. It needs no V8,
cloud credentials, or separate study checkout. Its lockfile pins the same fixed
SQLite 3.51.3 bundled by the main workspace.

Run the additional regression tests and the 23-check object-store suite:

```sh
cargo +1.94.1 test --manifest-path tools/local-store-tests/Cargo.toml --locked
cargo +1.94.1 run --manifest-path tools/local-store-tests/Cargo.toml --locked \
  --bin conformance -- local /tmp/conformance.sqlite3 /tmp/conformance.json
```

Use a fresh database/report path for each qualification. The conformance report
includes the tested source path and SQLite runtime version. The normal suite
includes cross-thread conditional-write races, object-store integration utilities,
canonical escaped keys, HEAD, random ranges, multipart ordering, paginated lists
under mutations, and the actual LTX adapter. Regression tests cover legacy format
migration, stable reads during overwrite/deletion/reclamation, shared copies,
missing multipart parts, abort races, abandoned-upload expiration, and large
common-prefix groups.

The explicit one-GiB test requires at least 2 GiB of free local disk and is kept
out of normal CI. It streams 128 eight-MiB parts, then validates HEAD, boundaries,
suffix, every byte of the full read, and persistence after reopen. Capture process
RSS using `time`, or run the resulting executable in a memory-limited container:

```sh
cargo +1.94.1 build --manifest-path tools/local-store-tests/Cargo.toml --locked --bin large-object
/usr/bin/time -v tools/local-store-tests/target/debug/large-object /tmp/one-gib.sqlite3
```

The standalone process measurement establishes storage-backend memory use. Radio
still needs its own full Celld/R2 application smoke test and memory limit check.
