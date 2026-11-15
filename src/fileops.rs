use std::fs;
use std::io;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::config::Operation;

/// Places `source` at `destination` using `operation`, never overwriting an existing file.
pub fn transfer(source: &Path, destination: &Path, operation: Operation) -> Result<()> {
    if destination.symlink_metadata().is_ok() {
        bail!("destination already exists: {}", destination.display());
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let result = match operation {
        Operation::Copy => fs::copy(source, destination).map(drop),
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
