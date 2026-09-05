// Copyright 2026 Deno Land Inc. Apache-2.0 license.

//! Offline, verified transfers of a complete Celld object-store namespace.
// Operator filesystem access is outside the runtime execution boundary.
#![allow(clippy::disallowed_methods)]

use anyhow::{bail, Context, Result};
use celld::cli_output::{Format, Output};
use std::path::PathBuf;

#[path = "store_copy/transfer.rs"]
mod transfer;

const HELP: &str = "\
Copy or verify one complete, QUIESCENT Celld object-store namespace.

USAGE:
  celld-store-copy copy --source SPEC --destination SPEC --manifest FILE --quiesced [--resume]
  celld-store-copy verify --source SPEC --destination SPEC --manifest FILE
  celld-store-copy audit --source SPEC

SPEC is az://CONTAINER[/PREFIX] or sqlite:///absolute/path/objects.sqlite3.
Azure credentials and Azurite configuration use the same environment as celld.

copy refuses a nonempty destination. --resume requires the existing manifest,
verifies its source snapshot and every existing destination object, and only
adds missing objects. It never deletes or deliberately overwrites an object.

--quiesced confirms all application nodes, background writers, deployment tools,
and other copy processes are stopped for BOTH namespaces. The tool also holds
Celld's runtime lock for each local database. Do not restart writers until it exits.

The manifest records exact keys, SHA-256, sizes, all object-store attributes,
source ETags and source modification times. Destination ETags and modification
times are regenerated. Source or destination drift fails verification.
See docs/store-copy.md before migrating a fleet.
";

fn parse(arguments: impl Iterator<Item = String>) -> Result<Option<transfer::Options>> {
    let mut arguments = arguments;
    let Some(command) = arguments.next() else {
        return Ok(None);
    };
    if matches!(command.as_str(), "--help" | "-h" | "help") {
        return Ok(None);
    }
    let mode = match command.as_str() {
        "copy" => transfer::Mode::Copy,
        "verify" => transfer::Mode::Verify,
        _ => bail!("unknown command {command:?}; use --help"),
    };
    let mut source = None;
    let mut destination = None;
    let mut manifest = None;
    let mut resume = false;
    let mut quiesced = false;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--source" => source = Some(arguments.next().context("--source requires a value")?),
            "--destination" => {
                destination = Some(arguments.next().context("--destination requires a value")?)
            }
            "--manifest" => {
                manifest = Some(PathBuf::from(
                    arguments.next().context("--manifest requires a value")?,
                ))
            }
            "--resume" => resume = true,
            "--quiesced" => quiesced = true,
            _ => bail!("unknown option {argument:?}; use --help"),
        }
    }
    if mode == transfer::Mode::Copy && !quiesced {
        bail!("copy requires --quiesced; stop both namespaces' writers first");
    }
    if mode == transfer::Mode::Verify && resume {
        bail!("--resume applies only to copy");
    }
    Ok(Some(transfer::Options {
        mode,
        source: source.context("missing --source")?,
        destination: destination.context("missing --destination")?,
        manifest: manifest.context("missing --manifest")?,
        resume,
    }))
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.first().is_some_and(|value| value == "audit") {
        if arguments.len() != 3 || arguments[1] != "--source" {
            bail!("usage: celld-store-copy audit --source SPEC");
        }
        let audit = transfer::audit(&arguments[2]).await?;
        Output::new(Format::Json)
            .line(format_args!("{}", serde_json::to_string_pretty(&audit)?))?;
        anyhow::ensure!(
            audit.folded_roots_sealed_or_absent,
            "unsealed or malformed node roots require investigation before migration"
        );
        return Ok(());
    }
    let Some(options) = parse(arguments.into_iter())? else {
        Output::new(Format::Text).help(HELP)?;
        return Ok(());
    };
    let summary = transfer::run(options).await?;
    Output::new(Format::Json).line(format_args!("{}", serde_json::to_string_pretty(&summary)?))?;
    Ok(())
}
