use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::config::Operation;

/// Places `source` at `destination` using `operation`, never overwriting an existing file.
/// `Remux` copies: remuxing a video is `remux::remux`'s job, and its sidecars are copied.
pub fn transfer(source: &Path, destination: &Path, operation: Operation) -> Result<()> {
    if destination.symlink_metadata().is_ok() {
        bail!("destination already exists: {}", destination.display());
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let result = match operation {
        Operation::Copy | Operation::Remux => fs::copy(source, destination).map(drop),
        Operation::Move => move_file(source, destination),
        Operation::Hardlink => fs::hard_link(source, destination),
        Operation::Symlink => symlink(source, destination),
        Operation::Reflink => reflink_copy::reflink_or_copy(source, destination).map(drop),
    };
    result.with_context(|| {
        format!(
            "cannot {operation:?} {} to {}",
            source.display(),
            destination.display()
        )
        .to_lowercase()
    })
}

/// A hidden name beside `destination` to build it under, so that it only appears once complete.
pub fn partial_path(destination: &Path) -> PathBuf {
    let stem = destination
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    let name = match destination.extension() {
        Some(extension) => format!(".{stem}.partial.{}", extension.to_string_lossy()),
        None => format!(".{stem}.partial"),
    };
    destination.with_file_name(name)
}

fn move_file(source: &Path, destination: &Path) -> io::Result<()> {
    match fs::rename(source, destination) {
        Err(err) if err.kind() == io::ErrorKind::CrossesDevices => {
            fs::copy(source, destination)?;
            fs::remove_file(source)
        }
        other => other,
    }
}

#[cfg(unix)]
fn symlink(source: &Path, destination: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(fs::canonicalize(source)?, destination)
}

#[cfg(windows)]
fn symlink(source: &Path, destination: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_file(fs::canonicalize(source)?, destination)
}

/// Removes `start` and then each parent while they are empty, stopping before `root`.
pub fn prune_empty_dirs(start: &Path, root: &Path) {
    let mut dir = start;
    while dir != root && dir.starts_with(root) && fs::remove_dir(dir).is_ok() {
        match dir.parent() {
            Some(parent) => dir = parent,
            None => break,
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use super::*;

    fn source(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("source.mkv");
        fs::write(&path, "video").unwrap();
        path
    }

    #[test]
    fn copy_keeps_the_source_and_creates_parents() {
        let dir = tempfile::tempdir().unwrap();
        let source = source(dir.path());
        let destination = dir.path().join("a/b/copy.mkv");
        transfer(&source, &destination, Operation::Copy).unwrap();
        assert_eq!(fs::read_to_string(&destination).unwrap(), "video");
        assert!(source.exists());
        assert_ne!(
            fs::metadata(&source).unwrap().ino(),
            fs::metadata(&destination).unwrap().ino()
        );
    }

    #[test]
    fn move_removes_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let source = source(dir.path());
        let destination = dir.path().join("moved.mkv");
        transfer(&source, &destination, Operation::Move).unwrap();
        assert_eq!(fs::read_to_string(&destination).unwrap(), "video");
        assert!(!source.exists());
    }

    #[test]
    fn hardlink_shares_an_inode() {
        let dir = tempfile::tempdir().unwrap();
        let source = source(dir.path());
        let destination = dir.path().join("link.mkv");
        transfer(&source, &destination, Operation::Hardlink).unwrap();
        assert_eq!(
            fs::metadata(&source).unwrap().ino(),
            fs::metadata(&destination).unwrap().ino()
        );
    }

    #[test]
    fn symlink_points_at_the_canonical_source() {
        let dir = tempfile::tempdir().unwrap();
        let source = source(dir.path());
        let destination = dir.path().join("symlink.mkv");
        transfer(&source, &destination, Operation::Symlink).unwrap();
        assert!(destination.symlink_metadata().unwrap().is_symlink());
        assert_eq!(
            fs::read_link(&destination).unwrap(),
            fs::canonicalize(&source).unwrap()
        );
        assert_eq!(fs::read_to_string(&destination).unwrap(), "video");
    }

    #[test]
    fn reflink_falls_back_to_a_copy() {
        let dir = tempfile::tempdir().unwrap();
        let source = source(dir.path());
        let destination = dir.path().join("reflink.mkv");
        transfer(&source, &destination, Operation::Reflink).unwrap();
        assert_eq!(fs::read_to_string(&destination).unwrap(), "video");
        assert!(source.exists());
    }

    #[test]
    fn never_overwrites_an_existing_destination() {
        let dir = tempfile::tempdir().unwrap();
        let source = source(dir.path());
        let destination = dir.path().join("taken.mkv");
        fs::write(&destination, "precious").unwrap();
        for operation in [
            Operation::Copy,
            Operation::Move,
            Operation::Hardlink,
            Operation::Symlink,
            Operation::Reflink,
        ] {
            let error = transfer(&source, &destination, operation).unwrap_err();
            assert!(
                error.to_string().contains("already exists"),
                "{operation:?}"
            );
        }
        assert_eq!(fs::read_to_string(&destination).unwrap(), "precious");
        assert!(source.exists());
    }

    #[test]
    fn never_replaces_a_dangling_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let source = source(dir.path());
        let destination = dir.path().join("dangling.mkv");
        std::os::unix::fs::symlink(dir.path().join("gone"), &destination).unwrap();
        assert!(transfer(&source, &destination, Operation::Copy).is_err());
    }

    #[test]
    fn a_missing_source_is_an_error_naming_both_paths() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("out.mkv");
        let error = transfer(
            &dir.path().join("missing.mkv"),
            &destination,
            Operation::Copy,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("missing.mkv") && message.contains("out.mkv"),
            "{message}"
        );
        assert!(!destination.exists());
    }

    #[test]
    fn pruning_removes_empty_directories_up_to_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("library");
        let deep = root.join("Show/Season 01");
        fs::create_dir_all(&deep).unwrap();
        prune_empty_dirs(&deep, &root);
        assert!(root.is_dir(), "the root itself stays");
        assert!(!root.join("Show").exists());
    }

    #[test]
    fn pruning_stops_at_a_directory_with_contents() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("library");
        let deep = root.join("Show/Season 01");
        fs::create_dir_all(&deep).unwrap();
        fs::write(root.join("Show/poster.jpg"), "").unwrap();
        prune_empty_dirs(&deep, &root);
        assert!(!deep.exists());
        assert!(root.join("Show/poster.jpg").exists());
    }

    #[test]
    fn pruning_ignores_directories_outside_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("library");
        let elsewhere = dir.path().join("elsewhere/empty");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&elsewhere).unwrap();
        prune_empty_dirs(&elsewhere, &root);
        assert!(elsewhere.exists());
    }
}
