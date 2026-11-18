//! The `remux` import operation, which hands each video to videoconverter.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use anyhow::{Context, Result, anyhow, bail};

/// Returned when the user pressed Ctrl-C during a remux.
#[derive(Debug)]
pub struct Interrupted;

impl fmt::Display for Interrupted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("interrupted")
    }
}

impl std::error::Error for Interrupted {}

/// Remuxes `source` into Matroska at `destination`, never overwriting an existing file. The
/// output is written beside `destination` under a hidden name and only renamed into place once
/// videoconverter succeeds, so a failed or interrupted remux leaves nothing in the library.
pub fn remux(program: &str, args: &[String], source: &Path, destination: &Path) -> Result<()> {
    if destination.symlink_metadata().is_ok() {
        bail!("destination already exists: {}", destination.display());
    }
    let parent = destination
        .parent()
        .context("destination has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("cannot create {}", parent.display()))?;
    let partial = partial_path(destination);
    let _ = fs::remove_file(&partial);

    let result = run(program, args, source, &partial).and_then(|status| {
        if status.success() {
            fs::rename(&partial, destination)
                .with_context(|| format!("cannot rename {}", partial.display()))
        } else if interrupted(status) {
            Err(Interrupted.into())
        } else {
            Err(anyhow!("{program} failed ({status}) on {}", source.display()))
        }
    });
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result
}

fn run(program: &str, args: &[String], source: &Path, output: &Path) -> Result<ExitStatus> {
    let mut child = Command::new(program)
        .args(["--yes", "--copy-video", "--no-external-subs"])
        .args(args)
        .arg("--output")
        .arg(output)
        .arg("--")
        .arg(source)
        .stdin(Stdio::null())
        .spawn()
        .with_context(|| format!("cannot run {program}"))?;
    // Ctrl-C reaches videoconverter too; reel waits for it to stop and then cleans up.
    let _ignoring = IgnoreInterrupts::start();
    child
        .wait()
        .with_context(|| format!("cannot wait for {program}"))
}

fn partial_path(destination: &Path) -> PathBuf {
    let stem = destination.file_stem().unwrap_or_default().to_string_lossy();
    destination.with_file_name(format!(".{stem}.partial.mkv"))
}

fn interrupted(status: ExitStatus) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if status.signal() == Some(libc::SIGINT) {
            return true;
        }
    }
    status.code() == Some(130)
}

/// Ignores SIGINT for as long as it is alive.
struct IgnoreInterrupts {
    #[cfg(unix)]
    previous: libc::sighandler_t,
}

impl IgnoreInterrupts {
    fn start() -> Self {
        IgnoreInterrupts {
            // SAFETY: replacing the SIGINT disposition is process-wide but has no other effects.
            #[cfg(unix)]
            previous: unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) },
        }
    }
}

impl Drop for IgnoreInterrupts {
    fn drop(&mut self) {
        // SAFETY: as in `start`, restoring the handler that was replaced there.
        #[cfg(unix)]
        unsafe {
            libc::signal(libc::SIGINT, self.previous);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// A stand-in for videoconverter that runs `body` with the output path in `$out`.
    fn fake(dir: &Path, body: &str) -> String {
        let path = dir.join("videoconverter");
        let script = format!(
            "#!/bin/sh\necho \"$@\" > \"$0.args\"\n\
             while [ \"$1\" != --output ]; do shift; done\nout=$2\n{body}\n"
        );
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_successful_remux_lands_at_the_destination() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake(dir.path(), "echo remuxed > \"$out\"");
        let destination = dir.path().join("Film (2000)/Film (2000).mkv");
        remux(&program, &[], Path::new("source.mp4"), &destination).unwrap();
        assert_eq!(fs::read_to_string(&destination).unwrap(), "remuxed\n");
        assert_eq!(entries(destination.parent().unwrap()), ["Film (2000).mkv"]);
    }

    #[test]
    fn a_failed_remux_leaves_no_files() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake(dir.path(), "echo partial > \"$out\"; exit 1");
        let destination = dir.path().join("out/Film.mkv");
        let err = remux(&program, &[], Path::new("source.mp4"), &destination).unwrap_err();
        assert!(!err.is::<Interrupted>());
        assert!(entries(&dir.path().join("out")).is_empty());
    }

    #[test]
    fn exit_status_130_means_interrupted() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake(dir.path(), "echo partial > \"$out\"; exit 130");
        let destination = dir.path().join("Film.mkv");
        let err = remux(&program, &[], Path::new("source.mp4"), &destination).unwrap_err();
        assert!(err.is::<Interrupted>());
        assert!(!partial_path(&destination).exists());
    }

    #[test]
    fn existing_destinations_are_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake(dir.path(), "echo remuxed > \"$out\"");
        let destination = dir.path().join("Film.mkv");
        fs::write(&destination, "original").unwrap();
        assert!(remux(&program, &[], Path::new("source.mp4"), &destination).is_err());
        assert_eq!(fs::read_to_string(&destination).unwrap(), "original");
    }

    #[test]
    fn extra_arguments_come_before_the_output_and_source() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake(dir.path(), "touch \"$out\"");
        let destination = dir.path().join("Film.mkv");
        remux(&program, &["--anime".into()], Path::new("in.mkv"), &destination).unwrap();
        let args = fs::read_to_string(format!("{program}.args")).unwrap();
        let expected = format!(
            "--yes --copy-video --no-external-subs --anime --output {} -- in.mkv\n",
            partial_path(&destination).display()
        );
        assert_eq!(args, expected);
    }
}
