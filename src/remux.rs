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

/// One file for `remux`: a source, and where its Matroska goes.
pub struct Job<'a> {
    pub source: &'a Path,
    pub destination: &'a Path,
}

/// Remuxes every job in one videoconverter run, which converts them in parallel, never
/// overwriting an existing file. Each output is written beside its destination under a hidden
/// name and renamed into place only once its conversion has finished, so a failed or interrupted
/// remux leaves nothing in the library. Returns each job's outcome in order; jobs that Ctrl-C cut
/// short fail with `Interrupted`.
pub fn remux(program: &str, args: &[String], jobs: &[Job]) -> Vec<Result<()>> {
    let mut outcomes: Vec<Result<()>> = jobs.iter().map(|job| prepare(job.destination)).collect();
    let ready: Vec<&Job> = jobs
        .iter()
        .zip(&outcomes)
        .filter(|(_, outcome)| outcome.is_ok())
        .map(|(job, _)| job)
        .collect();
    if ready.is_empty() {
        return outcomes;
    }
    let status = run(program, args, &ready);
    for (job, outcome) in jobs.iter().zip(&mut outcomes) {
        if outcome.is_ok() {
            *outcome = finish(program, job, &status);
        }
    }
    outcomes
}

fn prepare(destination: &Path) -> Result<()> {
    if destination.symlink_metadata().is_ok() {
        bail!("destination already exists: {}", destination.display());
    }
    let parent = destination
        .parent()
        .context("destination has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("cannot create {}", parent.display()))?;
    let _ = fs::remove_file(partial_path(destination));
    Ok(())
}

fn finish(program: &str, job: &Job, status: &Result<ExitStatus>) -> Result<()> {
    let partial = partial_path(job.destination);
    // videoconverter deletes the outputs it didn't finish, unless it was killed before it could.
    let finished = matches!(status, Ok(status) if status.code().is_some()) && partial.exists();
    let result = match status {
        _ if finished => fs::rename(&partial, job.destination)
            .with_context(|| format!("cannot rename {}", partial.display())),
        Ok(status) if interrupted(*status) => Err(Interrupted.into()),
        Ok(status) if status.success() => Err(anyhow!(
            "{program} produced nothing for {}",
            job.source.display()
        )),
        Ok(status) => Err(anyhow!("{program} failed ({status}) on {}", job.source.display())),
        Err(err) => Err(anyhow!("{err:#}")),
    };
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result
}

/// The oldest videoconverter with the options `remux` passes.
const MINIMUM_VERSION: (u32, u32) = (0, 5);

/// Fails unless `program` runs and is a videoconverter new enough for `remux`.
pub fn check(program: &str) -> Result<()> {
    let output = Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .with_context(|| format!("cannot run {program}, which the remux operation needs"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().next().unwrap_or_default().trim();
    let (major, minor) = MINIMUM_VERSION;
    match parse_version(line) {
        Some(version) if version >= MINIMUM_VERSION => Ok(()),
        Some(_) => bail!(
            "the remux operation needs videoconverter {major}.{minor} or later, but {program} is {line:?}"
        ),
        None => bail!("cannot tell which videoconverter {program} is from its version {line:?}"),
    }
}

/// Reads `videoconverter 0.5.0` as `(0, 5)`.
fn parse_version(line: &str) -> Option<(u32, u32)> {
    let version = line.strip_prefix("videoconverter ")?;
    let mut numbers = version.split('.').map(str::parse);
    Some((numbers.next()?.ok()?, numbers.next()?.ok()?))
}

fn run(program: &str, args: &[String], jobs: &[&Job]) -> Result<ExitStatus> {
    let mut command = Command::new(program);
    command
        .args(["--yes", "--copy-video", "--no-external-subs"])
        .args(args);
    for job in jobs {
        command.arg("--output").arg(partial_path(job.destination));
    }
    let mut child = command
        .arg("--")
        .args(jobs.iter().map(|job| job.source))
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

    /// A stand-in for videoconverter that logs its arguments to `<program>.args`, then runs
    /// `body` once per output with the output path in `$out` and the input in `$in`.
    fn fake(dir: &Path, body: &str) -> String {
        let path = dir.join("videoconverter");
        let script = format!(
            "#!/bin/sh\necho \"$@\" > \"$0.args\"\n: > \"$0.outs\"\n\
             while [ \"$1\" != -- ]; do\n\
             [ \"$1\" = --output ] && printf '%s\\n' \"$2\" >> \"$0.outs\"\nshift\ndone\nshift\n\
             status=0\nwhile IFS= read -r out; do in=$1; shift; {body}\ndone < \"$0.outs\"\n\
             exit $status\n"
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

    fn one(program: &str, args: &[String], destination: &Path) -> Result<()> {
        let job = Job {
            source: Path::new("source.mp4"),
            destination,
        };
        remux(program, args, &[job]).pop().unwrap()
    }

    fn version_script(dir: &Path, output: &str) -> String {
        let path = dir.join("videoconverter");
        fs::write(&path, format!("#!/bin/sh\necho stty noise >&2\n{output}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn versions_from_0_5_pass_the_check() {
        let dir = tempfile::tempdir().unwrap();
        for version in ["0.5.0", "0.12.1", "1.0.0"] {
            let program = version_script(dir.path(), &format!("echo videoconverter {version}"));
            check(&program).unwrap();
        }
    }

    #[test]
    fn older_versions_fail_the_check_naming_both() {
        let dir = tempfile::tempdir().unwrap();
        let program = version_script(dir.path(), "echo videoconverter 0.4.0");
        let err = format!("{:#}", check(&program).unwrap_err());
        assert!(err.contains("0.5 or later") && err.contains("0.4.0"), "{err}");
    }

    #[test]
    fn other_programs_and_missing_ones_fail_the_check() {
        let dir = tempfile::tempdir().unwrap();
        let program = version_script(dir.path(), "echo ffmpeg version 8.0");
        assert!(check(&program).is_err());
        let missing = dir.path().join("missing").to_string_lossy().into_owned();
        assert!(check(&missing).is_err());
    }

    #[test]
    fn a_successful_remux_lands_at_the_destination() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake(dir.path(), "echo remuxed > \"$out\"");
        let destination = dir.path().join("Film (2000)/Film (2000).mkv");
        one(&program, &[], &destination).unwrap();
        assert_eq!(fs::read_to_string(&destination).unwrap(), "remuxed\n");
        assert_eq!(entries(destination.parent().unwrap()), ["Film (2000).mkv"]);
    }

    #[test]
    fn one_run_remuxes_every_job_and_reports_each() {
        let dir = tempfile::tempdir().unwrap();
        // Like videoconverter, it fails overall when one input fails and deletes that output.
        let program = fake(
            dir.path(),
            "if [ \"$in\" = bad.mp4 ]; then status=1; else echo \"$in\" > \"$out\"; fi",
        );
        let [a, bad, c] = ["a.mkv", "bad.mkv", "c.mkv"].map(|name| dir.path().join(name));
        let jobs = [("a.mp4", &a), ("bad.mp4", &bad), ("c.mp4", &c)].map(|(source, destination)| {
            Job {
                source: Path::new(source),
                destination,
            }
        });
        let outcomes = remux(&program, &[], &jobs);
        assert!(outcomes[0].is_ok() && outcomes[2].is_ok());
        assert!(format!("{:#}", outcomes[1].as_ref().unwrap_err()).contains("bad.mp4"));
        assert_eq!(fs::read_to_string(&c).unwrap(), "c.mp4\n");
        assert!(!bad.exists() && !partial_path(&bad).exists());
        let args = fs::read_to_string(format!("{program}.args")).unwrap();
        assert_eq!(args.matches("--output").count(), 3, "{args}");
        assert!(args.ends_with("-- a.mp4 bad.mp4 c.mp4\n"), "{args}");
    }

    #[test]
    fn a_failed_remux_leaves_no_files() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake(dir.path(), "status=1");
        let destination = dir.path().join("out/Film.mkv");
        let err = one(&program, &[], &destination).unwrap_err();
        assert!(!err.is::<Interrupted>());
        assert!(entries(&dir.path().join("out")).is_empty());
    }

    #[test]
    fn interrupted_remuxes_fail_and_finished_ones_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake(
            dir.path(),
            "[ \"$in\" = a.mp4 ] && echo done > \"$out\"; status=130",
        );
        let [a, b] = ["a.mkv", "b.mkv"].map(|name| dir.path().join(name));
        let jobs = [("a.mp4", &a), ("b.mp4", &b)].map(|(source, destination)| Job {
            source: Path::new(source),
            destination,
        });
        let outcomes = remux(&program, &[], &jobs);
        assert!(outcomes[0].is_ok() && a.exists());
        assert!(outcomes[1].as_ref().unwrap_err().is::<Interrupted>());
    }

    #[test]
    fn existing_destinations_are_never_overwritten_or_passed_on() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake(dir.path(), "echo remuxed > \"$out\"");
        let destination = dir.path().join("Film.mkv");
        fs::write(&destination, "original").unwrap();
        assert!(one(&program, &[], &destination).is_err());
        assert_eq!(fs::read_to_string(&destination).unwrap(), "original");
        assert!(!Path::new(&format!("{program}.args")).exists(), "nothing left to run");
    }

    #[test]
    fn extra_arguments_come_before_the_outputs_and_sources() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake(dir.path(), "touch \"$out\"");
        let destination = dir.path().join("Film.mkv");
        one(&program, &["--anime".into()], &destination).unwrap();
        let args = fs::read_to_string(format!("{program}.args")).unwrap();
        let expected = format!(
            "--yes --copy-video --no-external-subs --anime --output {} -- source.mp4\n",
            partial_path(&destination).display()
        );
        assert_eq!(args, expected);
    }
}
