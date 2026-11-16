# Standalone embedded Worker runtime qualification

Run the existing bounded Worker runner with the `runtime` suite:

```sh
node tools/crypto-tests/run.mjs /absolute/candidate/celld /absolute/native/esbuild runtime
```

The same sandbox requirements documented in `../crypto-tests/README.md` apply:
isolated loopback, read-only inputs, scratch-only writes, an empty environment
and explicit CPU, physical memory, process, disk, file and wall-clock limits.
Never use an existing bucket or replica directory. The runner creates and removes
its own scratch state and prints the executable SHA-256 with results.

Checks cover CLI version/help success; the default no-argument runtime command;
standalone SQLite authority; R2 full reads, HEAD, metadata, bounded/suffix ranges,
listing and multipart reads; Durable Object SQL/KV; and runtime-only secret
overrides that must not appear on a subsequent launch without the override.
Only disposable literal test values are used.

After the first acknowledged SQL write, the runner sends SIGKILL, deletes the
scratch replica directory and restarts against the same bucket. It waits twelve
seconds for the old ten-second lease to expire, then proves SQL and R2 state
survive from bucket authority. This is a single-node bucket-durability crash test,
not a multi-node fleet-replication proof or an application acceptance test.
