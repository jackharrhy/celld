// Copyright 2026 Deno Land Inc. Apache-2.0 license.

//! Configuration and process ownership for a standalone, single-node store.

// This module is the process/filesystem boundary, before the node executor.
#![allow(clippy::disallowed_methods)]

use anyhow::Context;
use std::fs::File;
use std::path::{Path, PathBuf};

/// Parse the local storage specification. The path is literal, absolute, and
/// names one database; URI authorities, escapes, queries and prefixes are not
/// supported. Cloud specifications return `None` and use the existing parser.
pub fn path_from_spec(spec: &str) -> anyhow::Result<Option<PathBuf>> {
    let Some(scheme) = spec
        .get(..9)
        .filter(|s| s.eq_ignore_ascii_case("sqlite://"))
    else {
        anyhow::ensure!(
            !spec
                .get(..7)
                .is_some_and(|s| s.eq_ignore_ascii_case("sqlite:")),
            "local storage requires sqlite:///absolute/path/objects.sqlite3"
        );
        return Ok(None);
    };
    let path = &spec[scheme.len()..];
    anyhow::ensure!(
        path.starts_with('/')
            && !path.starts_with("//")
            && !path.ends_with('/')
            && !path.contains(['?', '#', '%', '\0'])
            && Path::new(path).file_name().is_some(),
        "local storage requires sqlite:///absolute/path/objects.sqlite3 without an authority, URI escapes, query or fragment"
    );
    Ok(Some(PathBuf::from(path)))
}

/// Hold for the entire runtime or offline migration. Operators such as deploy
/// may still use SQLite concurrently. Never remove this sidecar while a process
/// can hold it: its inode, not its contents, carries the operating-system lock.
pub fn lock_runtime(database: &Path) -> anyhow::Result<File> {
    let parent = database
        .parent()
        .context("local store has no parent directory")?;
    std::fs::create_dir_all(parent).context("create local storage directory")?;
    let database = if database.exists() {
        database
            .canonicalize()
            .context("resolve local storage database")?
    } else {
        parent.canonicalize()?.join(
            database
                .file_name()
                .context("local store needs a filename")?,
        )
    };
    let mut lock_path = database.into_os_string();
    lock_path.push(".runtime.lock");
    let mut options = File::options();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let guard = options
        .open(PathBuf::from(lock_path))
        .context("open local storage runtime lock")?;
    guard
        .try_lock()
        .context("local storage is already in use by a Celld node or offline migration")?;
    Ok(guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_specs_are_absolute_and_unambiguous() {
        assert_eq!(
            path_from_spec("SQLITE:///var/lib/celld/objects.sqlite3").unwrap(),
            Some(PathBuf::from("/var/lib/celld/objects.sqlite3"))
        );
        assert!(path_from_spec("az://radio/prefix").unwrap().is_none());
        for spec in [
            "sqlite:",
            "sqlite:/tmp/db",
            "sqlite://",
            "sqlite://host/tmp/db",
            "sqlite:////tmp/db",
            "sqlite:///",
            "sqlite:///tmp/",
            "sqlite:///tmp/db?mode=ro",
            "sqlite:///tmp/db#prefix",
            "sqlite:///tmp/a%20b",
        ] {
            assert!(path_from_spec(spec).is_err(), "{spec}");
        }
    }

    #[test]
    fn runtime_lock_excludes_another_owner_and_releases_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("objects.sqlite3");
        let guard = lock_runtime(&database).unwrap();
        assert!(lock_runtime(&database).is_err());
        // SQLite operators can still create/open the database while held.
        File::create(&database).unwrap();
        assert!(lock_runtime(&database).is_err());
        drop(guard);
        assert!(lock_runtime(&database).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn database_symlinks_share_the_runtime_lock() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("objects.sqlite3");
        File::create(&database).unwrap();
        let alias = dir.path().join("alias.sqlite3");
        std::os::unix::fs::symlink(&database, &alias).unwrap();
        let _guard = lock_runtime(&database).unwrap();
        assert!(lock_runtime(&alias).is_err());
    }
}
