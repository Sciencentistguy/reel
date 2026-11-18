//! The import pipeline: discover files, identify them, then file them into the library.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rustyline::error::ReadlineError;
use walkdir::WalkDir;

use crate::commands::human_duration;
use crate::config::{Config, MetadataConfig, Operation};
use crate::fileops;
use crate::library::Library;
use crate::metadata::{
    self, Candidate, EpisodeCount, EpisodeInfo, Length, MovieSource, ShowSource,
};
use crate::model::{Item, Kind, MediaInfo};
use crate::naming;
use crate::parse::{self, Parsed};
use crate::probe;
use crate::remux;
use crate::versions::{self, Choice, Resolve};

pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mkv", "mp4", "m4v", "avi", "mov", "wmv", "ts", "m2ts", "mpg", "mpeg", "webm", "flv", "ogv",
    "divx",
];

/// Plex's local-extras folder names; their contents are not films or episodes in their own right.
const EXTRAS_DIRECTORIES: &[&str] = &[
    "behind the scenes",
    "deleted scenes",
    "extras",
    "featurettes",
    "interviews",
    "other",
    "scenes",
    "shorts",
    "trailers",
];

#[derive(clap::Args)]
pub struct Options {
    /// Files or directories to import.
    #[arg(required = true)]
    pub paths: Vec<PathBuf>,
    /// How to place files in the library (overrides the configuration).
    #[arg(short, long, value_enum)]
    pub operation: Option<Operation>,
    /// An extra videoconverter argument for the remux operation, after any configured ones
    /// (`--remux-arg=--anime`). Repeatable.
    #[arg(long = "remux-arg", value_name = "ARG", allow_hyphen_values = true)]
    pub remux_args: Vec<String>,
    /// Never prompt; skip anything without a confident match.
    #[arg(short, long)]
    pub quiet: bool,
    /// Show what would happen without touching files or the database.
    #[arg(short = 'n', long, visible_alias = "dry-run")]
    pub pretend: bool,
    /// Treat every file as this kind instead of guessing from its name.
    #[arg(short, long, value_enum)]
    pub kind: Option<Kind>,
    /// Always ask which match to use, even when one is certain (overrides the configuration).
    #[arg(long, conflicts_with = "no_timid")]
    pub timid: bool,
    /// Auto-select confident matches even if the configuration says to be timid.
    #[arg(long)]
    pub no_timid: bool,
    /// Place episode files even when their running time doesn't fit the episode's runtime.
    #[arg(long)]
    pub skip_length_check: bool,
}

/// Options for `reel rescan`, which adopts files already in the library folders.
#[derive(clap::Args)]
pub struct Rescan {
    /// Directories to scan instead of the library's film and TV folders.
    pub paths: Vec<PathBuf>,
    /// Never prompt; skip anything without a confident match.
    #[arg(short, long)]
    pub quiet: bool,
    /// Show what would happen without touching the database.
    #[arg(short = 'n', long, visible_alias = "dry-run")]
    pub pretend: bool,
    /// Always ask which match to use, even when one is certain (overrides the configuration).
    #[arg(long, conflicts_with = "no_timid")]
    pub timid: bool,
    /// Auto-select confident matches even if the configuration says to be timid.
    #[arg(long)]
    pub no_timid: bool,
    /// Place episode files even when their running time doesn't fit the episode's runtime.
    #[arg(long)]
    pub skip_length_check: bool,
}

struct Pending {
    path: PathBuf,
    /// The directory given on the command line, which bounds empty-directory pruning.
    root: PathBuf,
    parsed: Parsed,
}

struct Group {
    kind: Kind,
    title: String,
    year: Option<i32>,
    id_hint: Option<(String, String)>,
    /// The running time of a film's only file, when known.
    length: Option<Length>,
    files: Vec<Pending>,
}

enum Decision {
    Use(Candidate),
    Skip,
    Abort,
}

enum Lookup<'a> {
    Movie(&'a dyn MovieSource),
    Show(&'a dyn ShowSource),
}

impl Lookup<'_> {
    fn source(&self) -> &'static str {
        match self {
            Lookup::Movie(_) => "tmdb",
            Lookup::Show(source) => source.name(),
        }
    }

    fn search(&self, title: &str, year: Option<i32>) -> Result<Vec<Candidate>> {
        match self {
            Lookup::Movie(source) => source.search_movies(title, year),
            Lookup::Show(source) => source.search_shows(title, year),
        }
    }

    fn by_id(&self, id: &str) -> Result<Candidate> {
        match self {
            Lookup::Movie(source) => source.movie(id),
            Lookup::Show(source) => source.show(id),
        }
    }
}

#[derive(Default)]
struct Summary {
    imported: usize,
    skipped: usize,
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

pub fn file_stamp(path: &Path) -> Result<(u64, i64)> {
    let metadata =
        std::fs::metadata(path).with_context(|| format!("cannot stat {}", path.display()))?;
    let mtime = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64);
    Ok((metadata.len(), mtime))
}

/// Reads a line with editing keys (arrows, history within the line, Ctrl-A/E, ...). Returns `None`
/// at end of input or on Ctrl-C.
pub fn prompt(text: &str) -> Result<Option<String>> {
    let mut editor = rustyline::DefaultEditor::new()?;
    match editor.readline(text) {
        Ok(line) => Ok(Some(line.trim().to_string())),
        Err(ReadlineError::Eof | ReadlineError::Interrupted) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| VIDEO_EXTENSIONS.contains(&e.to_lowercase().as_str()))
}

fn is_sample(path: &Path) -> bool {
    path.file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|stem| {
            stem.to_lowercase()
                .split(|c: char| !c.is_alphanumeric())
                .any(|word| word == "sample")
        })
}

fn in_extras_directory(path: &Path, root: &Path) -> bool {
    path.parent()
        .and_then(|dir| dir.strip_prefix(root).ok())
        .is_some_and(|relative| {
            relative.components().any(|component| {
                let name = component.as_os_str().to_string_lossy().to_lowercase();
                EXTRAS_DIRECTORIES.contains(&name.as_str())
            })
        })
}

fn collect(paths: &[PathBuf]) -> Result<Vec<(PathBuf, PathBuf)>> {
    let (files, ignored) = videos(paths)?;
    if ignored > 0 {
        println!("Ignoring {ignored} sample or extras file(s).");
    }
    Ok(files)
}

/// The video files at or under `paths`, canonicalised, each with the given path it was found
/// under, and how many samples and extras were left out.
pub fn videos(paths: &[PathBuf]) -> Result<(Vec<(PathBuf, PathBuf)>, usize)> {
    let mut files = Vec::new();
    let mut ignored = 0;
    for path in paths {
        let path = std::fs::canonicalize(path)
            .with_context(|| format!("cannot access {}", path.display()))?;
        if path.is_file() {
            let root = path.parent().unwrap_or(Path::new("/")).to_path_buf();
            files.push((path, root));
            continue;
        }
        for entry in WalkDir::new(&path).sort_by_file_name() {
            let entry = entry?;
            if !entry.file_type().is_file() || !is_video(entry.path()) {
                continue;
            }
            if is_sample(entry.path()) || in_extras_directory(entry.path(), &path) {
                ignored += 1;
                continue;
            }
            files.push((entry.into_path(), path.clone()));
        }
    }
    Ok((files, ignored))
}

fn group(files: Vec<Pending>) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    let mut index: HashMap<(Kind, String, Option<i32>), usize> = HashMap::new();
    for file in files {
        let parsed = &file.parsed;
        let identity = match &parsed.id_hint {
            Some((source, id)) => format!("{source}-{id}"),
            None => parsed.title.to_lowercase(),
        };
        let key = (parsed.kind, identity, parsed.year);
        let position = *index.entry(key).or_insert_with(|| {
            groups.push(Group {
                kind: parsed.kind,
                title: parsed.title.clone(),
                year: parsed.year,
                id_hint: parsed.id_hint.clone(),
                length: None,
                files: Vec::new(),
            });
            groups.len() - 1
        });
        groups[position].files.push(file);
    }
    groups
}

fn describe(title: &str, year: Option<i32>) -> String {
    match year {
        Some(year) => format!("{title} ({year})"),
        None => title.to_string(),
    }
}

const OVERVIEW_LENGTH: usize = 300;
const DETAIL_INDENT: &str = "     ";
const MAX_DETAIL_WIDTH: usize = 100;

/// A candidate's original title and overview, dimmed beneath it, then a blank line to set it
/// apart from the next.
fn print_details(candidate: &Candidate) {
    if let Some(original) = &candidate.original_title {
        println!(
            "{DETAIL_INDENT}{}",
            paint("2", &format!("Original title: {original}"))
        );
    }
    if let Some(overview) = &candidate.overview {
        let width = textwrap::termwidth().min(MAX_DETAIL_WIDTH) - DETAIL_INDENT.len();
        for line in textwrap::wrap(&truncate(overview, OVERVIEW_LENGTH), width) {
            println!("{DETAIL_INDENT}{}", paint("2", &line));
        }
    }
    println!();
}

/// Collapses whitespace and cuts `text` at a word boundary within `width` characters.
fn truncate(text: &str, width: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= width {
        return text;
    }
    let cut: String = text.chars().take(width).collect();
    let kept = cut.rsplit_once(' ').map_or(cut.as_str(), |(head, _)| head);
    format!("{}…", kept.trim_end_matches([',', ';', ':', '.', '-']))
}

/// The candidate's title and year, as a terminal hyperlink to its web page when stdout is a terminal.
fn describe_candidate(candidate: &Candidate) -> String {
    let text = describe(&candidate.title, candidate.year);
    if io::stdout().is_terminal() {
        hyperlink(&text, &candidate.url)
    } else {
        text
    }
}

/// Wraps `text` in an OSC 8 escape sequence linking it to `url`.
fn hyperlink(text: &str, url: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
}

/// Whether output may be coloured: it is a terminal, and `NO_COLOR` isn't set.
fn use_colour() -> bool {
    io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

/// `text` in the SGR style `code` (such as `"33"` for yellow), when output may be coloured.
pub fn paint(code: &str, text: &str) -> String {
    if use_colour() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// A warning about one file: the problem, then the file's path beneath it, dimmed.
fn warn_about(path: &Path, problem: &str) {
    eprintln!("  {} {problem}", paint("33", "warning:"));
    eprintln!("    {}", paint("2", &path.display().to_string()));
}

fn match_percent(distance: f64) -> String {
    colour_percent(((1.0 - distance) * 100.0).round(), use_colour())
}

/// `N file(s) unmatched`, in red when `colour` is set.
fn unmatched_note(count: usize, colour: bool) -> String {
    let text = format!("{count} file(s) unmatched");
    if colour {
        format!("\x1b[31m{text}\x1b[0m")
    } else {
        text
    }
}

/// Formats a match percentage, coloured by how trustworthy it is when `colour` is set.
fn colour_percent(percent: f64, colour: bool) -> String {
    let text = format!("{percent:.0}%");
    if !colour {
        return text;
    }
    let code = match percent {
        p if p >= 95.0 => 32,
        p if p >= 90.0 => 92,
        p if p >= 75.0 => 33,
        _ => 31,
    };
    format!("\x1b[{code}m{text}\x1b[0m")
}

/// How many of the best candidates have their running times or episode lists fetched.
const DETAIL_LOOKUPS: usize = 3;
/// The least a show loses from its match for having any unmatched files.
const UNMATCHED_PENALTY: f64 = 0.02;

/// The running time of a film group's file, if it is a single, whole film.
fn measure(group: &Group) -> Result<Option<Length>> {
    let [file] = group.files.as_slice() else {
        return Ok(None);
    };
    if group.kind != Kind::Movie || file.parsed.part.is_some() {
        return Ok(None);
    }
    Ok(probe::duration(&file.path)?.map(|seconds| Length {
        seconds,
        alternate_cut: file.parsed.edition.is_some(),
    }))
}

/// Fills in the running times of the best candidates, which searches leave out, and re-ranks.
fn with_runtimes(
    lookup: &Lookup,
    title: &str,
    year: Option<i32>,
    length: Option<Length>,
    ranked: Vec<(f64, Candidate)>,
) -> Vec<(f64, Candidate)> {
    let candidates = ranked
        .into_iter()
        .enumerate()
        .map(|(position, (_, mut candidate))| {
            if position < DETAIL_LOOKUPS && candidate.runtime.is_none() {
                match lookup.by_id(&candidate.id) {
                    Ok(full) => candidate.runtime = full.runtime,
                    Err(err) => eprintln!(
                        "  warning: cannot fetch the running time of {}-{}: {err:#}",
                        candidate.source, candidate.id
                    ),
                }
            }
            candidate
        })
        .collect();
    metadata::rank(title, year, length, candidates)
}

/// Whether a show group has episode numbers or air dates to look for in episode lists.
fn has_placements(group: &Group) -> bool {
    group.kind == Kind::Episode
        && group.files.iter().any(|file| {
            file.parsed.air_date.is_some()
                || (file.parsed.season.is_some() && file.parsed.episode.is_some())
        })
}

/// Extra distance for a show whose episode list lacks any of the files' episodes, such as a
/// miniseries sharing its name with a long-running show: a flat amount, so a single unmatched
/// file shows in the match percentage, plus more the larger the share unmatched.
fn missing_episode_penalty(files: &[Pending], episodes: &[EpisodeInfo]) -> f64 {
    let checked: Vec<bool> = files
        .iter()
        .filter_map(|file| listed(&file.parsed, episodes))
        .collect();
    if checked.is_empty() {
        return 0.0;
    }
    let missing = checked.iter().filter(|&&found| !found).count() as f64 / checked.len() as f64;
    if missing == 0.0 {
        0.0
    } else {
        UNMATCHED_PENALTY + missing / 2.0
    }
}

/// Whether `episodes` has the episode, or every episode, a file names; `None` when the file
/// names none.
fn listed(parsed: &Parsed, episodes: &[EpisodeInfo]) -> Option<bool> {
    if let Some(date) = &parsed.air_date {
        return Some(episodes.iter().any(|e| e.air_date.as_ref() == Some(date)));
    }
    let (season, first) = (parsed.season?, parsed.episode?);
    let last = parsed.episode_end.unwrap_or(first).max(first);
    Some((first..=last).all(|number| {
        episodes
            .iter()
            .any(|e| e.season == season && e.number == number)
    }))
}

/// How many files name an episode that `candidate`'s fetched episode list lacks; all of them
/// with a placement when the list hasn't been fetched.
fn unlisted(files: &[Pending], candidate: &Candidate) -> usize {
    let episodes = candidate.episodes.as_deref().unwrap_or_default();
    files
        .iter()
        .filter(|file| listed(&file.parsed, episodes) == Some(false))
        .count()
}

/// Whether `candidate` can be chosen without asking: for a show, every file's episode must be
/// in its episode list, or the match is wrong.
fn covers(group: &Group, candidate: &Candidate) -> bool {
    !has_placements(group)
        || (candidate.episodes.is_some() && unlisted(&group.files, candidate) == 0)
}

fn is_confident(group: &Group, ranked: &[(f64, Candidate)], threshold: f64) -> bool {
    metadata::is_strong(ranked, threshold)
        && ranked
            .first()
            .is_some_and(|(_, candidate)| covers(group, candidate))
}

/// Fetches the episode lists of the best `lookups` candidates not yet checked, penalises those
/// lacking the files' episodes, and re-ranks.
fn with_episode_lists(
    source: &dyn ShowSource,
    files: &[Pending],
    ranked: Vec<(f64, Candidate)>,
    lookups: usize,
) -> Vec<(f64, Candidate)> {
    let mut ranked: Vec<_> = ranked
        .into_iter()
        .enumerate()
        .map(|(position, (distance, mut candidate))| {
            if position >= lookups || candidate.episodes.is_some() {
                return (distance, candidate);
            }
            match source.episodes(&candidate.id, None) {
                Ok(episodes) => {
                    let penalty = missing_episode_penalty(files, &episodes);
                    candidate.episodes = Some(episodes);
                    ((distance + penalty).min(1.0), candidate)
                }
                Err(err) => {
                    eprintln!(
                        "  warning: cannot fetch the episodes of {}-{}: {err:#}",
                        candidate.source, candidate.id
                    );
                    (distance, candidate)
                }
            }
        })
        .collect();
    ranked.sort_by(|a, b| a.0.total_cmp(&b.0));
    ranked
}

/// Ranks candidates by name and year, then by running time when that is uncertain. A show's
/// best candidate always has its episode list checked, and the next few too when uncertain or
/// when the best lacks the files' episodes.
fn rank_thoroughly(
    group: &Group,
    lookup: &Lookup,
    title: &str,
    year: Option<i32>,
    candidates: Vec<Candidate>,
    check: impl Fn(&[(f64, Candidate)]) -> bool,
) -> Vec<(f64, Candidate)> {
    let mut ranked = metadata::rank(title, year, group.length, candidates);
    if group.length.is_some() && check(&ranked) {
        ranked = with_runtimes(lookup, title, year, group.length, ranked);
    }
    if let Lookup::Show(source) = lookup
        && has_placements(group)
    {
        let uncertain = check(&ranked);
        let lookups = if uncertain { DETAIL_LOOKUPS } else { 1 };
        ranked = with_episode_lists(*source, &group.files, ranked, lookups);
        if !uncertain && !ranked.first().is_some_and(|(_, best)| covers(group, best)) {
            ranked = with_episode_lists(*source, &group.files, ranked, DETAIL_LOOKUPS);
        }
    }
    ranked
}

/// Prints one candidate as `<label> Title (year) [source-id] (match%, runtime)` and its details.
fn print_candidate(
    label: &str,
    candidate: &Candidate,
    files: &[Pending],
    distance: f64,
    in_library: i64,
) {
    let known = match in_library {
        0 => String::new(),
        count => format!(" - already in library ({count} file(s))"),
    };
    let runtime = candidate.runtime.map_or(String::new(), |minutes| {
        format!(", {}", human_duration(f64::from(minutes) * 60.0))
    });
    let episodes = candidate.episodes.as_deref().map_or(String::new(), |list| {
        match unlisted(files, candidate) {
            0 => format!(", {}", EpisodeCount::of(list)),
            missing => format!(
                ", {}, {}",
                EpisodeCount::of(list),
                unmatched_note(missing, use_colour())
            ),
        }
    });
    println!(
        "  {label} {} [{}-{}] ({}{runtime}{episodes}){known}",
        paint("1", &describe_candidate(candidate)),
        candidate.source,
        candidate.id,
        match_percent(distance)
    );
    print_details(candidate);
}

/// How a file is listed when asked for: a film's filename is enough to recognise it, while a
/// show's episodes may be spread across folders.
fn file_label(kind: Kind, path: &Path) -> String {
    match kind {
        Kind::Movie => path.file_name().unwrap_or(path.as_os_str()),
        Kind::Episode => path.as_os_str(),
    }
    .to_string_lossy()
    .into_owned()
}

fn decide(
    group: &Group,
    lookup: &Lookup,
    library: &Library,
    threshold: f64,
    interactive: bool,
    timid: bool,
    edition: &mut Option<Option<String>>,
) -> Result<Decision> {
    let mut named_by_id = None;
    if let Some((source, id)) = &group.id_hint
        && source == lookup.source()
    {
        let found = lookup.by_id(id).map(|candidate| match lookup {
            Lookup::Show(source) if has_placements(group) => {
                with_episode_lists(*source, &group.files, vec![(0.0, candidate)], 1).remove(0)
            }
            _ => (0.0, candidate),
        });
        match found {
            Ok(named) if timid => named_by_id = Some(named),
            Ok((distance, candidate)) if !covers(group, &candidate) => {
                println!(
                    "  {source}-{id}: {} ({}).",
                    unmatched_note(unlisted(&group.files, &candidate), use_colour()),
                    match_percent(distance)
                );
                named_by_id = Some((distance, candidate));
            }
            Ok((_, candidate)) => {
                println!(
                    "  Matched by ID: {} [{source}-{id}]",
                    describe_candidate(&candidate)
                );
                return Ok(Decision::Use(candidate));
            }
            Err(err) => eprintln!("  warning: lookup of {source}-{id} failed: {err:#}"),
        }
    }

    let (mut title, mut year) = (group.title.clone(), group.year);
    let mut searched_manually = false;
    loop {
        let mut ranked = rank_thoroughly(
            group,
            lookup,
            &title,
            year,
            lookup.search(&title, year)?,
            |ranked| timid || searched_manually || !metadata::is_strong(ranked, threshold),
        );
        if let Some((distance, named)) = named_by_id.take() {
            ranked.retain(|(_, found)| (found.source, &found.id) != (named.source, &named.id));
            ranked.insert(0, (distance, named));
        }
        if !timid && !searched_manually && is_confident(group, &ranked, threshold) {
            let (distance, candidate) = ranked.into_iter().next().unwrap();
            println!(
                "  Matched: {} [{}-{}] ({})",
                describe_candidate(&candidate),
                candidate.source,
                candidate.id,
                match_percent(distance)
            );
            return Ok(Decision::Use(candidate));
        }
        if !interactive {
            println!("  Not choosing a match without asking; skipping.");
            return Ok(Decision::Skip);
        }

        let shown = &ranked[..ranked.len().min(8)];
        if shown.is_empty() {
            println!("  No candidates found for {:?}.", describe(&title, year));
        } else if let Some(length) = group.length {
            println!("  The file runs {}.", human_duration(length.seconds));
        }
        let in_library = shown
            .iter()
            .map(|(_, candidate)| {
                library.count_identified(group.kind, candidate.source, &candidate.id)
            })
            .collect::<Result<Vec<_>>>()?;
        for (number, ((distance, candidate), count)) in shown.iter().zip(&in_library).enumerate() {
            print_candidate(
                &format!("{}.", number + 1),
                candidate,
                &group.files,
                *distance,
                *count,
            );
        }
        loop {
            let default = if shown.is_empty() {
                "s".to_string()
            } else {
                let first_known = in_library.iter().position(|&count| count > 0);
                (first_known.unwrap_or(0) + 1).to_string()
            };
            let edition_choice = if group.kind == Kind::Movie {
                ", edi[t]ion"
            } else {
                ""
            };
            let Some(answer) = prompt(&format!(
                "  # selection, [s]kip, [e]nter search, enter [i]d{edition_choice}, [p]aths, a[b]ort? [{default}] "
            ))?
            else {
                return Ok(Decision::Abort);
            };
            let answer = if answer.is_empty() {
                default.clone()
            } else {
                answer.to_lowercase()
            };
            match answer.as_str() {
                "s" => return Ok(Decision::Skip),
                "b" => return Ok(Decision::Abort),
                "p" => {
                    for file in &group.files {
                        println!("    {}", file_label(group.kind, &file.path));
                    }
                }
                "e" => {
                    let Some(new_title) = prompt("  Title: ")? else {
                        return Ok(Decision::Abort);
                    };
                    if new_title.is_empty() {
                        continue;
                    }
                    let new_year = prompt("  Year (blank for any): ")?.unwrap_or_default();
                    title = new_title;
                    year = new_year.parse().ok();
                    searched_manually = true;
                    break;
                }
                "t" if group.kind == Kind::Movie => {
                    let Some(answer) = prompt("  Edition (blank for none): ")? else {
                        return Ok(Decision::Abort);
                    };
                    let name = Some(answer).filter(|answer| !answer.is_empty());
                    match &name {
                        Some(name) => println!("  Edition set to {name:?}."),
                        None => println!("  Edition cleared."),
                    }
                    *edition = Some(name);
                }
                "i" => {
                    let Some(id) = prompt(&format!("  {} ID: ", lookup.source()))? else {
                        return Ok(Decision::Abort);
                    };
                    let candidate = match lookup.by_id(&id) {
                        Ok(candidate) => candidate,
                        Err(err) => {
                            eprintln!("  {err:#}");
                            continue;
                        }
                    };
                    let ranked =
                        rank_thoroughly(group, lookup, &title, year, vec![candidate], |_| true);
                    let (distance, candidate) = &ranked[0];
                    let count =
                        library.count_identified(group.kind, candidate.source, &candidate.id)?;
                    print_candidate("Found:", candidate, &group.files, *distance, count);
                    if !timid && is_confident(group, &ranked, threshold) {
                        return Ok(Decision::Use(ranked.into_iter().next().unwrap().1));
                    }
                    let Some(answer) = prompt("  Use this match? [Y/n] ")? else {
                        return Ok(Decision::Abort);
                    };
                    if answer.is_empty() || answer.eq_ignore_ascii_case("y") {
                        return Ok(Decision::Use(ranked.into_iter().next().unwrap().1));
                    }
                }
                other => match other.parse::<usize>() {
                    Ok(number) if (1..=shown.len()).contains(&number) => {
                        return Ok(Decision::Use(shown[number - 1].1.clone()));
                    }
                    _ => println!("  Unrecognised choice."),
                },
            }
        }
    }
}

fn base_item(group: &Group, file: &Pending, candidate: &Candidate) -> Item {
    let mut item = Item::new(group.kind, file.path.clone(), candidate.title.clone());
    item.year = candidate.year.or(group.year);
    item.source = Some(candidate.source.to_string());
    item.source_id = Some(candidate.id.clone());
    item
}

fn fetch_episodes(source: &dyn ShowSource, id: &str, files: &[Pending]) -> Vec<EpisodeInfo> {
    let fetch = |season: Option<u32>| {
        source.episodes(id, season).unwrap_or_else(|err| {
            let scope = season.map_or("episode list".to_string(), |s| format!("season {s}"));
            eprintln!("  warning: cannot fetch {scope}: {err:#}");
            Vec::new()
        })
    };
    if files.iter().any(|file| file.parsed.air_date.is_some()) {
        return fetch(None);
    }
    let seasons: BTreeSet<u32> = files.iter().filter_map(|file| file.parsed.season).collect();
    seasons.into_iter().flat_map(|s| fetch(Some(s))).collect()
}

/// Fills in the episode fields of `item` from the episodes its name gives, checking `duration`
/// (in seconds) against their runtimes. Returns `false` when the file cannot be placed.
fn apply_episode(
    item: &mut Item,
    parsed: &Parsed,
    episodes: &[EpisodeInfo],
    duration: Option<f64>,
) -> bool {
    item.season = parsed.season;
    item.episode = parsed.episode;
    item.episode_end = parsed.episode_end;
    item.air_date = parsed.air_date.clone();
    item.date_based = parsed.air_date.is_some();

    if let Some(date) = &parsed.air_date {
        let Some(found) = episodes.iter().find(|e| e.air_date.as_ref() == Some(date)) else {
            warn_about(
                &item.path,
                &format!("no episode of the show aired on {date}"),
            );
            return false;
        };
        if !fits(item, duration, &[found]) {
            return false;
        }
        item.season = Some(found.season);
        item.episode = Some(found.number);
        item.episode_title = found.title.clone();
        return true;
    }

    let (Some(season), Some(first)) = (parsed.season, parsed.episode) else {
        return false;
    };
    let last = parsed.episode_end.unwrap_or(first);
    let mut found = Vec::new();
    for number in first..=last {
        let Some(episode) = episodes
            .iter()
            .find(|e| e.season == season && e.number == number)
        else {
            warn_about(
                &item.path,
                &format!("s{season:02}e{number:02} is not in the show's episode list"),
            );
            return false;
        };
        found.push(episode);
    }
    if !fits(item, duration, &found) {
        return false;
    }
    let mut titles: Vec<&str> = Vec::new();
    for title in found.iter().filter_map(|episode| episode.title.as_deref()) {
        if !titles.contains(&title) {
            titles.push(title);
        }
    }
    item.episode_title = Some(titles.join(" & ")).filter(|t| !t.is_empty());
    item.air_date = found[0].air_date.clone();
    true
}

/// What became of an episode file that `place_episode` was asked to place.
#[derive(Debug, PartialEq)]
enum Placement {
    Placed,
    Skipped,
    /// The user interrupted the prompt (Ctrl-C or Ctrl-D) to stop importing altogether.
    Aborted,
}

/// Places an episode file as its name says or, failing that and given a way to `ask`, as the
/// episode the user names: by number (`s13e05`, `13x05`) or air date, after searching the
/// episode titles if they like.
fn place_episode(
    item: &mut Item,
    parsed: &Parsed,
    episodes: &[EpisodeInfo],
    duration: Option<f64>,
    ask: Option<impl Fn(&str) -> Result<Option<String>>>,
) -> Result<Placement> {
    if apply_episode(item, parsed, episodes, duration) {
        return Ok(Placement::Placed);
    }
    let skip = || {
        println!("    {}", paint("2", "Skipped."));
        Ok(Placement::Skipped)
    };
    let Some(ask) = ask.filter(|_| !episodes.is_empty()) else {
        return skip();
    };
    println!(
        "    {}",
        paint(
            "2",
            "Which episode is it? Give its number (s13e05) or air date, or search by title or year."
        )
    );
    println!("    {}", paint("2", "Leave blank to skip."));
    loop {
        let Some(answer) = ask("    episode> ")? else {
            return Ok(Placement::Aborted);
        };
        if answer.is_empty() {
            return skip();
        }
        let given = parse::parse_name(&format!("Show {answer}"));
        if given.air_date.is_some() || (given.season.is_some() && given.episode.is_some()) {
            let mut named = parsed.clone();
            (named.season, named.episode) = (given.season, given.episode);
            (named.episode_end, named.air_date) = (given.episode_end, given.air_date);
            if apply_episode(item, &named, episodes, duration) {
                let code = item.episode_code().unwrap_or_default();
                println!("    {}", paint("32", &format!("Placed as {code}.")));
                return Ok(Placement::Placed);
            }
            continue;
        }
        let words = answer.to_lowercase();
        let found: Vec<_> = episodes
            .iter()
            .filter(|episode| {
                let title = episode.title.as_ref();
                title.is_some_and(|title| title.to_lowercase().contains(&words))
                    || episode
                        .air_date
                        .as_ref()
                        .is_some_and(|date| date.starts_with(&words))
            })
            .collect();
        if found.is_empty() {
            println!(
                "      {}",
                paint(
                    "2",
                    &format!("No episode's title or air date matches {answer:?}.")
                )
            );
        }
        for episode in found.iter().take(SEARCH_RESULTS) {
            println!("      {}", describe_listed(episode));
        }
        if found.len() > SEARCH_RESULTS {
            let more = found.len() - SEARCH_RESULTS;
            println!(
                "      {}",
                paint("2", &format!("… and {more} more; search more narrowly."))
            );
        }
    }
}

/// How many episodes a title search lists.
const SEARCH_RESULTS: usize = 20;

/// `s13e05 Summer Solstice (2005-03-27, 1h 30m)`, for picking an episode from a list, with the
/// number in bold and the details dimmed.
fn describe_listed(episode: &EpisodeInfo) -> String {
    let mut details = Vec::new();
    details.extend(episode.air_date.clone());
    details.extend(
        episode
            .runtime
            .map(|minutes| human_duration(f64::from(minutes) * 60.0)),
    );
    let code = format!("s{:02}e{:02}", episode.season, episode.number);
    let mut text = paint("1", &code);
    if let Some(title) = &episode.title {
        text += &format!(" {title}");
    }
    if !details.is_empty() {
        text += &format!(" {}", paint("2", &format!("({})", details.join(", "))));
    }
    text
}

/// Whether a file running `duration` seconds can be `episodes`, warning when it can't.
fn fits(item: &Item, duration: Option<f64>, episodes: &[&EpisodeInfo]) -> bool {
    if metadata::episodes_fit(duration, episodes) {
        return true;
    }
    let runtime = metadata::episodes_runtime(episodes).unwrap_or_default();
    warn_about(
        &item.path,
        &format!(
            "the file runs {} but {} runs {}",
            human_duration(duration.unwrap_or_default()),
            describe_episodes(episodes),
            human_duration(f64::from(runtime) * 60.0),
        ),
    );
    false
}

/// `s05e28 Change Your Mind`, or `s01e01-e02` for several.
pub fn describe_episodes(episodes: &[&EpisodeInfo]) -> String {
    let (Some(first), Some(last)) = (episodes.first(), episodes.last()) else {
        return String::new();
    };
    let mut text = format!("s{:02}e{:02}", first.season, first.number);
    if last.number != first.number {
        text += &format!("-e{:02}", last.number);
    }
    if let [only] = episodes
        && let Some(title) = &only.title
    {
        text += &format!(" {title}");
    }
    text
}

struct Importer<'a> {
    config: &'a Config,
    library: &'a mut Library,
    operation: Operation,
    pretend: bool,
    probe: bool,
    /// Record files where they are instead of moving them to their proper names.
    adopt: bool,
    /// What a pretend run would have added, so later files still see earlier ones as versions.
    planned: Vec<Item>,
    /// Destinations already assigned this run, which a pretend run has not yet created.
    claimed: HashSet<PathBuf>,
    versioned: bool,
    summary: Summary,
    /// videoconverter arguments for the remux operation.
    remux_args: Vec<String>,
    /// Remuxes waiting until every group has been decided, so prompts never wait on them.
    queued: Vec<QueuedRemux>,
    /// Asks what to do with a new version of something already in the library. Without it,
    /// both are kept.
    resolve: Option<Resolve<'a>>,
}

struct QueuedRemux {
    item: Item,
    source: PathBuf,
    destination: PathBuf,
    /// The library file this remux replaces once it has finished.
    replaces: Option<Item>,
}

/// What became of one file.
enum Outcome {
    Imported,
    Skipped,
    /// The user stopped the import.
    Aborted,
}

impl Importer<'_> {
    /// Starts probing `files` in the background, unless probing is off.
    fn start_probing(&self, files: &[Pending]) -> Option<probe::Background> {
        self.probe
            .then(|| probe::Background::start(files.iter().map(|file| file.path.clone()).collect()))
    }

    /// The `index`th file's media information, or `None` when it cannot be probed. Stops probing
    /// altogether when ffprobe cannot be run.
    fn media(&mut self, probes: &mut Option<probe::Background>, index: usize) -> Option<MediaInfo> {
        let err = match probes.as_mut()?.get(index) {
            Ok(media) => return Some(media),
            Err(err) => err,
        };
        eprintln!("  warning: {err:#}");
        if err.downcast_ref::<io::Error>().is_some() {
            eprintln!("  warning: continuing without media information");
            self.probe = false;
            *probes = None;
        }
        None
    }

    fn import(&mut self, mut item: Item, file: &Pending, media: Option<MediaInfo>) -> Result<Outcome> {
        if let Some(media) = media {
            item.media = media;
        }
        if item.media.resolution.is_none() {
            item.media.resolution = file.parsed.resolution.clone();
        }
        (item.size, item.mtime) = file_stamp(&file.path)?;
        if !self.adopt {
            item.original_stamp = Some((item.size, item.mtime));
        }

        if self.library.has_duplicate(&item)?
            || self
                .planned
                .iter()
                .any(|planned| planned.is_duplicate_of(&item))
        {
            eprintln!(
                "  warning: {} is already in the library; skipping",
                file.path.display()
            );
            return Ok(Outcome::Skipped);
        }

        let mut replacing = None;
        let in_library = match self.resolve.filter(|_| !self.adopt) {
            Some(resolve) => {
                let mut existing = self.library.versions_of(&item)?;
                if !existing.is_empty() {
                    match resolve(&item, &file.path, &existing)? {
                        Choice::KeepBoth => {}
                        Choice::Skip => return Ok(Outcome::Skipped),
                        Choice::Abort => return Ok(Outcome::Aborted),
                        Choice::Replace(index) => replacing = Some(existing.remove(index)),
                    }
                }
                !existing.is_empty()
            }
            None => self.library.has_other_versions(&item)?,
        };
        let has_versions =
            in_library || self.planned.iter().any(|planned| planned.same_work(&item));
        let remuxing = self.operation == Operation::Remux && !self.adopt;
        if remuxing {
            item.path.set_extension("mkv");
        }
        let replaced_path = replacing.as_ref().map(|old: &Item| old.path.clone());
        let claimed = &self.claimed;
        // The file being replaced is going, so its name is free for the new one.
        let taken = |path: &Path| {
            claimed.contains(path)
                || (path.symlink_metadata().is_ok() && Some(path) != replaced_path.as_deref())
        };
        let destination = if self.adopt {
            file.path.clone()
        } else {
            naming::free_destination(self.config, &item, has_versions, &file.path, taken)
        };
        let in_place = destination == file.path;
        if !in_place && taken(&destination) {
            eprintln!(
                "  warning: {} already exists; skipping {}",
                destination.display(),
                file.path.display()
            );
            return Ok(Outcome::Skipped);
        }

        let name = file.path.file_name().unwrap_or_default().to_string_lossy();
        if self.adopt {
            println!("  {name}");
        } else if in_place {
            println!("  {name} (already in place)");
        } else {
            println!("  {name}\n    -> {}", destination.display());
        }
        if let Some(old) = &replacing {
            println!("    replacing {}", old.path.display());
        }
        self.versioned |= has_versions;
        self.claimed.insert(destination.clone());
        if self.pretend {
            self.planned.push(item);
            return Ok(Outcome::Imported);
        }
        if remuxing && !in_place {
            self.planned.push(item.clone());
            self.queued.push(QueuedRemux {
                item,
                source: file.path.clone(),
                destination,
                replaces: replacing,
            });
            return Ok(Outcome::Imported);
        }

        if !in_place {
            let sidecars = if self.config.import.subtitles {
                naming::sidecars(&file.path)
            } else {
                Vec::new()
            };
            match &replacing {
                // Complete beside the old file first, so a failure leaves the old one alone.
                Some(_) => {
                    let partial = fileops::partial_path(&destination);
                    fileops::transfer(&file.path, &partial, self.operation)?;
                    if let Err(err) = fs::rename(&partial, &destination) {
                        // A moved source goes back where it was rather than being lost.
                        if self.operation != Operation::Move
                            || fs::rename(&partial, &file.path).is_err()
                        {
                            let _ = fs::remove_file(&partial);
                        }
                        return Err(err).with_context(|| {
                            format!("cannot rename {} into place", partial.display())
                        });
                    }
                }
                None => fileops::transfer(&file.path, &destination, self.operation)?,
            }
            // Before the new sidecars arrive, as they may take the old ones' names.
            if let Some(old) = &replacing {
                self.retire(old, &destination)?;
            }
            for (sidecar, suffix) in sidecars {
                let target = naming::sidecar_destination(&destination, &suffix);
                if let Err(err) = fileops::transfer(&sidecar, &target, self.operation) {
                    eprintln!("  warning: {err:#}");
                }
            }
            if self.operation == Operation::Move
                && let Some(parent) = file.path.parent()
            {
                fileops::prune_empty_dirs(parent, &file.root);
            }
        }
        if in_place && let Some(old) = &replacing {
            self.retire(old, &destination)?;
        }
        let original = (!self.adopt).then(|| file.path.clone());
        self.record(item, original, destination)?;
        Ok(Outcome::Imported)
    }

    /// Removes `old` from the library and deletes its files, all but `kept`, which the new file
    /// is about to take over.
    fn retire(&mut self, old: &Item, kept: &Path) -> Result<()> {
        let sidecars = naming::sidecars(&old.path).into_iter().map(|(path, _)| path);
        let video = (old.path != kept).then(|| old.path.clone());
        for path in sidecars.chain(video) {
            if let Err(err) = fs::remove_file(&path)
                && err.kind() != io::ErrorKind::NotFound
            {
                eprintln!("  warning: cannot delete {}: {err}", path.display());
            }
        }
        if old.path != kept
            && let Some(parent) = old.path.parent()
        {
            fileops::prune_empty_dirs(parent, naming::library_root(self.config, old.kind));
        }
        if let Some(id) = old.id {
            self.library.remove(id)?;
        }
        Ok(())
    }

    fn record(&mut self, mut item: Item, original: Option<PathBuf>, path: PathBuf) -> Result<()> {
        item.original_path = original;
        item.path = path;
        item.added = now();
        self.library.save(&mut item)
    }

    /// Runs the queued remuxes, all in one videoconverter run so they go in parallel.
    fn remux_queued(&mut self) {
        let queued = std::mem::take(&mut self.queued);
        if queued.is_empty() {
            return;
        }
        println!("\nRemuxing {} file(s)", queued.len());
        let jobs: Vec<_> = queued
            .iter()
            .map(|job| remux::Job {
                source: &job.source,
                destination: &job.destination,
                replace: job
                    .replaces
                    .as_ref()
                    .is_some_and(|old| old.path == job.destination),
            })
            .collect();
        let outcomes = remux::remux(&self.config.import.videoconverter, &self.remux_args, &jobs);
        let mut interrupted = 0;
        for (job, outcome) in queued.into_iter().zip(outcomes) {
            let root = naming::library_root(self.config, job.item.kind).to_path_buf();
            let parent = job.destination.parent().map(Path::to_path_buf);
            let source = job.source.clone();
            let Err(err) = outcome.and_then(|()| self.record_remux(job)) else {
                continue;
            };
            if let Some(parent) = parent {
                fileops::prune_empty_dirs(&parent, &root);
            }
            self.summary.imported -= 1;
            self.summary.skipped += 1;
            if err.is::<remux::Interrupted>() {
                interrupted += 1;
            } else {
                eprintln!("  error: {}: {err:#}", source.display());
            }
        }
        if interrupted > 0 {
            println!("Interrupted; {interrupted} file(s) left unimported.");
        }
    }

    /// Records a finished remux, describing the new file rather than its source.
    fn record_remux(&mut self, job: QueuedRemux) -> Result<()> {
        let QueuedRemux {
            mut item,
            source,
            destination,
            replaces,
        } = job;
        if let Some(old) = &replaces {
            self.retire(old, &destination)?;
        }
        if self.config.import.subtitles {
            for (sidecar, suffix) in naming::sidecars(&source) {
                let target = naming::sidecar_destination(&destination, &suffix);
                if let Err(err) = fileops::transfer(&sidecar, &target, Operation::Copy) {
                    eprintln!("  warning: {err:#}");
                }
            }
        }
        if self.probe {
            match probe::probe(&destination) {
                Ok(media) => {
                    let resolution = item.media.resolution.take();
                    item.media = media;
                    item.media.resolution = item.media.resolution.take().or(resolution);
                }
                Err(err) => eprintln!("  warning: {err:#}"),
            }
        }
        (item.size, item.mtime) = file_stamp(&destination)?;
        self.record(item, Some(source), destination)
    }
}

/// Where imports are looked up. Each source is only connected when first needed.
trait Sources {
    fn movies(&mut self) -> Result<&dyn MovieSource>;
    fn shows(&mut self) -> Result<&dyn ShowSource>;
}

/// The metadata services the configuration names.
struct Online<'a> {
    config: &'a MetadataConfig,
    movies: Option<Box<dyn MovieSource>>,
    shows: Option<Box<dyn ShowSource>>,
}

impl<'a> Online<'a> {
    fn new(config: &'a MetadataConfig) -> Self {
        Online {
            config,
            movies: None,
            shows: None,
        }
    }
}

impl Sources for Online<'_> {
    fn movies(&mut self) -> Result<&dyn MovieSource> {
        if self.movies.is_none() {
            self.movies = Some(metadata::movie_source(self.config)?);
        }
        Ok(self.movies.as_deref().unwrap())
    }

    fn shows(&mut self) -> Result<&dyn ShowSource> {
        if self.shows.is_none() {
            self.shows = Some(metadata::show_source(self.config)?);
        }
        Ok(self.shows.as_deref().unwrap())
    }
}

pub fn run(config: &Config, library: &mut Library, options: &Options) -> Result<()> {
    let mut sources = Online::new(&config.metadata);
    let ask = |new: &Item, path: &Path, existing: &[Item]| versions::ask(new, path, existing, prompt);
    let interactive = !options.quiet && io::stdin().is_terminal();
    run_with(config, library, options, false, &mut sources, interactive.then_some(&ask))
}

/// Adds files that are in the library folders but missing from the database, leaving them where
/// they are; `reel move` can rename them afterwards.
pub fn rescan(config: &Config, library: &mut Library, rescan: Rescan) -> Result<()> {
    let options = rescan_options(config, rescan)?;
    let mut sources = Online::new(&config.metadata);
    run_with(config, library, &options, true, &mut sources, None)
}

/// Import options that look in the library's own folders unless `rescan` names others.
fn rescan_options(config: &Config, rescan: Rescan) -> Result<Options> {
    let paths = if rescan.paths.is_empty() {
        let roots: Vec<_> = [&config.library.movies, &config.library.tv]
            .into_iter()
            .filter(|root| root.is_dir())
            .cloned()
            .collect();
        anyhow::ensure!(
            !roots.is_empty(),
            "neither {} nor {} exists",
            config.library.movies.display(),
            config.library.tv.display()
        );
        roots
    } else {
        rescan.paths
    };
    Ok(Options {
        paths,
        operation: None,
        remux_args: Vec::new(),
        quiet: rescan.quiet,
        pretend: rescan.pretend,
        kind: None,
        timid: rescan.timid,
        no_timid: rescan.no_timid,
        skip_length_check: rescan.skip_length_check,
    })
}

fn run_with(
    config: &Config,
    library: &mut Library,
    options: &Options,
    adopt: bool,
    sources: &mut dyn Sources,
    resolve: Option<Resolve>,
) -> Result<()> {
    let mut pending = Vec::new();
    let mut known = 0;
    for (path, root) in collect(&options.paths)? {
        // A remuxed file differs from its source, so the source is recognised where it was.
        if library.contains_path(&path)?
            || (!adopt && library.imported_from(&path, file_stamp(&path)?)?)
        {
            known += 1;
            continue;
        }
        let mut parsed = parse::parse(&path);
        if let Some(kind) = options.kind
            && parsed.kind != kind
        {
            if kind == Kind::Episode {
                eprintln!("warning: no episode number in {}; skipping", path.display());
                continue;
            }
            parsed.kind = Kind::Movie;
            (parsed.season, parsed.episode, parsed.episode_end) = (None, None, None);
            (parsed.air_date, parsed.episode_title) = (None, None);
        }
        if parsed.title.is_empty() {
            eprintln!(
                "warning: cannot work out a title for {}; skipping",
                path.display()
            );
            continue;
        }
        pending.push(Pending { path, root, parsed });
    }
    if known > 0 {
        println!("Skipping {known} file(s) already in the library.");
    }
    if pending.is_empty() {
        println!("Nothing to import.");
        return Ok(());
    }

    let operation = options.operation.unwrap_or(config.import.operation);
    // Found out before any prompt, rather than once per file after them all.
    if operation == Operation::Remux && !adopt && !options.pretend {
        remux::check(&config.import.videoconverter)?;
    }

    let interactive = !options.quiet && io::stdin().is_terminal();
    let mut importer = Importer {
        config,
        library,
        operation,
        pretend: options.pretend,
        probe: config.import.probe,
        adopt,
        planned: Vec::new(),
        claimed: HashSet::new(),
        versioned: false,
        summary: Summary::default(),
        remux_args: [&config.import.remux_args[..], &options.remux_args[..]].concat(),
        queued: Vec::new(),
        resolve,
    };

    let mut aborted = false;
    let mut can_measure = config.import.probe;
    'groups: for mut group in group(pending) {
        let label = match group.kind {
            Kind::Movie => "film",
            Kind::Episode => "show",
        };
        println!(
            "\n{} ({label}, {} file(s))",
            describe(&group.title, group.year),
            group.files.len()
        );
        // Probing overlaps with the metadata lookup and any prompt.
        let mut probes = importer.start_probing(&group.files);

        if can_measure {
            match measure(&group) {
                Ok(length) => group.length = length,
                Err(err) => can_measure = err.downcast_ref::<io::Error>().is_none(),
            }
        }

        let mut edition = None;
        let lookup = match group.kind {
            Kind::Movie => Lookup::Movie(sources.movies()?),
            Kind::Episode => Lookup::Show(sources.shows()?),
        };
        let decision = decide(
            &group,
            &lookup,
            importer.library,
            config.import.strong_match,
            interactive,
            (config.import.timid || options.timid) && !options.no_timid,
            &mut edition,
        )?;
        let mut candidate = match decision {
            Decision::Use(candidate) => candidate,
            Decision::Skip => {
                importer.summary.skipped += group.files.len();
                continue;
            }
            Decision::Abort => {
                aborted = true;
                break;
            }
        };

        let episodes = match lookup {
            Lookup::Show(source) => match candidate.episodes.take() {
                Some(episodes) => episodes,
                None => fetch_episodes(source, &candidate.id, &group.files),
            },
            Lookup::Movie(_) => Vec::new(),
        };
        for (index, file) in group.files.iter().enumerate() {
            let mut item = base_item(&group, file, &candidate);
            let media = importer.media(&mut probes, index);
            match group.kind {
                Kind::Movie => {
                    item.edition = edition
                        .clone()
                        .unwrap_or_else(|| file.parsed.edition.clone());
                    item.part = file.parsed.part;
                }
                Kind::Episode => {
                    let duration = media
                        .as_ref()
                        .and_then(|media| media.duration)
                        .filter(|_| !options.skip_length_check);
                    match place_episode(
                        &mut item,
                        &file.parsed,
                        &episodes,
                        duration,
                        interactive.then_some(prompt),
                    )? {
                        Placement::Placed => {}
                        Placement::Skipped => {
                            importer.summary.skipped += 1;
                            continue;
                        }
                        Placement::Aborted => {
                            aborted = true;
                            break 'groups;
                        }
                    }
                }
            }
            match importer.import(item, file, media) {
                Ok(Outcome::Imported) => importer.summary.imported += 1,
                Ok(Outcome::Skipped) => importer.summary.skipped += 1,
                Ok(Outcome::Aborted) => {
                    aborted = true;
                    break 'groups;
                }
                Err(err) => {
                    eprintln!("  error: {err:#}");
                    importer.summary.skipped += 1;
                }
            }
        }
    }

    if aborted && !importer.queued.is_empty() {
        let left = importer.queued.len();
        importer.summary.imported -= left;
        importer.summary.skipped += left;
        println!("Stopped before remuxing {left} file(s).");
    } else {
        importer.remux_queued();
    }

    let Summary { imported, skipped } = importer.summary;
    let verb = match (adopt, options.pretend) {
        (false, true) => "Would import",
        (false, false) => "Imported",
        (true, true) => "Would add",
        (true, false) => "Added",
    };
    println!("\n{verb} {imported} file(s); skipped {skipped}.");
    if importer.versioned {
        println!(
            "Some items now have several versions; `reel move` adds version suffixes to the older copies."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_percentages_by_quality() {
        let code = |percent| colour_percent(percent, true);
        assert_eq!(code(98.0), "\x1b[32m98%\x1b[0m");
        assert_eq!(code(95.0), "\x1b[32m95%\x1b[0m");
        assert_eq!(code(94.0), "\x1b[92m94%\x1b[0m");
        assert_eq!(code(90.0), "\x1b[92m90%\x1b[0m");
        assert_eq!(code(89.0), "\x1b[33m89%\x1b[0m");
        assert_eq!(code(75.0), "\x1b[33m75%\x1b[0m");
        assert_eq!(code(66.0), "\x1b[31m66%\x1b[0m");
        assert_eq!(colour_percent(98.0, false), "98%");
    }

    #[test]
    fn hyperlinks_use_osc_8() {
        assert_eq!(
            hyperlink("Dune (2021)", "https://example.com/1"),
            "\x1b]8;;https://example.com/1\x1b\\Dune (2021)\x1b]8;;\x1b\\"
        );
    }

    #[test]
    fn truncates_overviews_at_word_boundaries() {
        assert_eq!(truncate("A  short\nplot.", 50), "A short plot.");
        assert_eq!(truncate("one two, three four", 10), "one two…");
    }

    fn episodes() -> Vec<EpisodeInfo> {
        let episode = |season, number, title: &str, date: &str| EpisodeInfo {
            season,
            number,
            title: Some(title.to_string()),
            air_date: Some(date.to_string()),
            runtime: None,
        };
        vec![
            episode(1, 1, "Pilot (1)", "2010-01-01"),
            episode(1, 2, "Pilot (2)", "2010-01-01"),
            episode(8, 20, "Elijah Wood", "2011-11-15"),
        ]
    }

    fn applied(name: &str, episodes: &[EpisodeInfo]) -> Option<Item> {
        let parsed = parse::parse_name(name);
        let mut item = Item::new(Kind::Episode, "/in/file.mkv".into(), parsed.title.clone());
        apply_episode(&mut item, &parsed, episodes, None).then_some(item)
    }

    #[test]
    fn episode_titles_come_from_metadata() {
        let item = applied("Show S01E01-E02 Wrong Title", &episodes()).unwrap();
        assert_eq!(item.episode_title.as_deref(), Some("Pilot (1) & Pilot (2)"));
        assert_eq!(item.episode_code().as_deref(), Some("s01e01-e02"));
        assert_eq!(item.air_date.as_deref(), Some("2010-01-01"));
    }

    #[test]
    fn dates_resolve_to_seasons() {
        let item = applied("Show 2011.11.15 HDTV", &episodes()).unwrap();
        assert_eq!((item.season, item.episode), (Some(8), Some(20)));
        assert_eq!(item.episode_title.as_deref(), Some("Elijah Wood"));
        assert_eq!(item.episode_code().as_deref(), Some("2011-11-15"));
        assert!(applied("Show 2011.11.16 HDTV", &episodes()).is_none());
    }

    #[test]
    fn ignores_samples_and_extras() {
        assert!(is_sample(Path::new("/in/the.film-sample.mkv")));
        assert!(!is_sample(Path::new("/in/Sampler.Platter.2010.mkv")));
        assert!(in_extras_directory(
            Path::new("/in/Film/Featurettes/a.mkv"),
            Path::new("/in")
        ));
        assert!(!in_extras_directory(
            Path::new("/in/Film/a.mkv"),
            Path::new("/in")
        ));
    }

    use std::cell::{Cell, RefCell};

    use anyhow::anyhow;

    fn candidate(title: &str, year: i32, id: &str) -> Candidate {
        Candidate {
            source: "tmdb",
            id: id.into(),
            title: title.into(),
            year: Some(year),
            url: String::new(),
            original_title: None,
            overview: None,
            runtime: None,
            episodes: None,
        }
    }

    #[derive(Default)]
    struct StubMovies {
        results: Vec<Candidate>,
        by_id: Option<Candidate>,
        /// Full records by ID, which take precedence over `by_id`.
        records: Vec<Candidate>,
        fail_search: bool,
        searches: Cell<usize>,
        lookups: RefCell<Vec<String>>,
    }

    impl MovieSource for StubMovies {
        fn search_movies(&self, _title: &str, _year: Option<i32>) -> Result<Vec<Candidate>> {
            self.searches.set(self.searches.get() + 1);
            if self.fail_search {
                return Err(anyhow!("search unavailable"));
            }
            Ok(self.results.clone())
        }

        fn movie(&self, id: &str) -> Result<Candidate> {
            self.lookups.borrow_mut().push(id.to_string());
            self.records
                .iter()
                .find(|record| record.id == id)
                .or(self.by_id.as_ref())
                .cloned()
                .ok_or_else(|| anyhow!("not found"))
        }
    }

    #[derive(Default)]
    struct StubShows {
        fail: bool,
        requested: RefCell<Vec<Option<u32>>>,
    }

    impl ShowSource for StubShows {
        fn name(&self) -> &'static str {
            "tvdb"
        }

        fn search_shows(&self, _: &str, _: Option<i32>) -> Result<Vec<Candidate>> {
            Ok(Vec::new())
        }

        fn show(&self, _: &str) -> Result<Candidate> {
            Err(anyhow!("unused"))
        }

        fn episodes(&self, _: &str, season: Option<u32>) -> Result<Vec<EpisodeInfo>> {
            self.requested.borrow_mut().push(season);
            if self.fail {
                return Err(anyhow!("unavailable"));
            }
            Ok(vec![EpisodeInfo {
                season: season.unwrap_or(1),
                number: 1,
                title: Some("Title".into()),
                air_date: None,
                runtime: None,
            }])
        }
    }

    fn pending(path: &str) -> Pending {
        let path = PathBuf::from(path);
        Pending {
            root: PathBuf::from("/in"),
            parsed: parse::parse(&path),
            path,
        }
    }

    fn group_of(path: &str) -> Group {
        group(vec![pending(path)]).remove(0)
    }

    fn decision(
        group: &Group,
        source: &StubMovies,
        timid: bool,
        interactive: bool,
    ) -> Result<Decision> {
        decide(
            group,
            &Lookup::Movie(source),
            &Library::in_memory()?,
            0.08,
            interactive,
            timid,
            &mut None,
        )
    }

    #[test]
    fn films_are_listed_by_filename_and_episodes_by_path() {
        let path = Path::new("/in/Show/Season 1/Show.S01E01.mkv");
        assert_eq!(file_label(Kind::Movie, path), "Show.S01E01.mkv");
        assert_eq!(
            file_label(Kind::Episode, path),
            "/in/Show/Season 1/Show.S01E01.mkv"
        );
    }

    #[test]
    fn describes_titles_with_optional_years() {
        assert_eq!(describe("Heat", Some(1995)), "Heat (1995)");
        assert_eq!(describe("Heat", None), "Heat");
    }

    #[test]
    fn groups_files_of_the_same_work() {
        let groups = group(vec![
            pending("/in/Heat.1995.1080p.mkv"),
            pending("/in/heat 1995 2160p.mkv"),
            pending("/in/Heat.1996.mkv"),
            pending("/in/Heat.S01E01.mkv"),
            pending("/in/Heat.S01E02.mkv"),
        ]);
        let sizes: Vec<_> = groups
            .iter()
            .map(|group| (group.kind, group.year, group.files.len()))
            .collect();
        assert_eq!(
            sizes,
            [
                (Kind::Movie, Some(1995), 2),
                (Kind::Movie, Some(1996), 1),
                (Kind::Episode, None, 2),
            ]
        );
    }

    #[test]
    fn id_tags_group_files_regardless_of_their_titles() {
        let groups = group(vec![
            pending("/in/Heat {tmdb-949}.mkv"),
            pending("/in/Hete {tmdb-949}.mkv"),
            pending("/in/Heat {tmdb-1}.mkv"),
        ]);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].files.len(), 2);
        assert_eq!(groups[0].id_hint, Some(("tmdb".into(), "949".into())));
    }

    #[test]
    fn base_items_take_identity_from_the_match() {
        let group = group_of("/in/Heat.1996.mkv");
        let file = &group.files[0];
        let matched = base_item(&group, file, &candidate("Heat", 1995, "949"));
        assert_eq!(matched.year, Some(1995));
        assert_eq!(matched.source.as_deref(), Some("tmdb"));
        assert_eq!(matched.source_id.as_deref(), Some("949"));

        let mut undated = candidate("Heat", 1995, "949");
        undated.year = None;
        assert_eq!(base_item(&group, file, &undated).year, Some(1996));
    }

    #[test]
    fn a_clear_match_is_used_unprompted() {
        let source = StubMovies {
            results: vec![candidate("Heat", 1995, "949"), candidate("Heat", 1986, "2")],
            ..StubMovies::default()
        };
        let group = group_of("/in/Heat.1995.mkv");
        match decision(&group, &source, false, false).unwrap() {
            Decision::Use(found) => assert_eq!(found.id, "949"),
            _ => panic!("expected the 1995 film"),
        }
    }

    #[test]
    fn running_times_settle_films_of_the_same_name() {
        let with_runtime = |id, minutes| Candidate {
            runtime: Some(minutes),
            ..candidate("Heat", 1995, id)
        };
        let source = StubMovies {
            results: vec![
                candidate("Heat", 1995, "short"),
                candidate("Heat", 1995, "long"),
            ],
            records: vec![with_runtime("short", 90), with_runtime("long", 170)],
            ..StubMovies::default()
        };
        let mut group = group_of("/in/Heat.1995.mkv");
        assert!(matches!(
            decision(&group, &source, false, false).unwrap(),
            Decision::Skip
        ));
        assert!(source.lookups.borrow().is_empty());

        group.length = Some(Length {
            seconds: 168.0 * 60.0,
            alternate_cut: false,
        });
        match decision(&group, &source, false, false).unwrap() {
            Decision::Use(found) => assert_eq!(found.id, "long"),
            _ => panic!("expected the film whose running time matches"),
        }
        assert_eq!(*source.lookups.borrow(), ["short", "long"]);
    }

    #[test]
    fn ambiguous_matches_are_skipped_when_not_interactive() {
        let source = StubMovies {
            results: vec![candidate("Dune", 2021, "1"), candidate("Dune", 1984, "2")],
            ..StubMovies::default()
        };
        let group = group_of("/in/Dune.mkv");
        assert!(matches!(
            decision(&group, &source, false, false).unwrap(),
            Decision::Skip
        ));
    }

    #[test]
    fn timid_mode_never_applies_a_match_unprompted() {
        let source = StubMovies {
            results: vec![candidate("Heat", 1995, "949")],
            ..StubMovies::default()
        };
        let group = group_of("/in/Heat.1995.mkv");
        assert!(matches!(
            decision(&group, &source, true, false).unwrap(),
            Decision::Skip
        ));
    }

    #[test]
    fn no_candidates_means_skipping() {
        let group = group_of("/in/Heat.1995.mkv");
        assert!(matches!(
            decision(&group, &StubMovies::default(), false, false).unwrap(),
            Decision::Skip
        ));
    }

    #[test]
    fn a_weak_match_is_not_applied() {
        let source = StubMovies {
            results: vec![candidate("Something Else Entirely", 1995, "7")],
            ..StubMovies::default()
        };
        let group = group_of("/in/Heat.1995.mkv");
        assert!(matches!(
            decision(&group, &source, false, false).unwrap(),
            Decision::Skip
        ));
    }

    #[test]
    fn search_failures_propagate() {
        let source = StubMovies {
            fail_search: true,
            ..StubMovies::default()
        };
        let group = group_of("/in/Heat.1995.mkv");
        let error = decision(&group, &source, false, false).err().unwrap();
        assert!(error.to_string().contains("search unavailable"));
    }

    #[test]
    fn id_tags_skip_the_search() {
        let source = StubMovies {
            by_id: Some(candidate("Heat", 1995, "949")),
            ..StubMovies::default()
        };
        let group = group_of("/in/Heat {tmdb-949}.mkv");
        match decision(&group, &source, false, false).unwrap() {
            Decision::Use(found) => assert_eq!(found.id, "949"),
            _ => panic!("expected a direct match"),
        }
        assert_eq!(source.lookups.borrow().as_slice(), ["949"]);
        assert_eq!(source.searches.get(), 0);
    }

    #[test]
    fn timid_mode_still_searches_when_an_id_tag_is_present() {
        let source = StubMovies {
            by_id: Some(candidate("Heat", 1995, "949")),
            ..StubMovies::default()
        };
        let group = group_of("/in/Heat {tmdb-949}.mkv");
        assert!(matches!(
            decision(&group, &source, true, false).unwrap(),
            Decision::Skip
        ));
        assert_eq!(source.searches.get(), 1);
    }

    #[test]
    fn a_failed_id_lookup_falls_back_to_searching() {
        let source = StubMovies {
            results: vec![candidate("Heat", 1995, "949")],
            ..StubMovies::default()
        };
        let group = group_of("/in/Heat.1995 {tmdb-1}.mkv");
        match decision(&group, &source, false, false).unwrap() {
            Decision::Use(found) => assert_eq!(found.id, "949"),
            _ => panic!("expected the search result"),
        }
        assert_eq!(source.lookups.borrow().as_slice(), ["1"]);
    }

    #[test]
    fn id_tags_for_another_source_are_ignored() {
        let source = StubMovies {
            results: vec![candidate("Heat", 1995, "949")],
            ..StubMovies::default()
        };
        let group = group_of("/in/Heat.1995 {tvdb-1}.mkv");
        assert!(matches!(
            decision(&group, &source, false, false).unwrap(),
            Decision::Use(_)
        ));
        assert!(source.lookups.borrow().is_empty());
    }

    #[test]
    fn episodes_are_fetched_once_per_season() {
        let shows = StubShows::default();
        let files = [
            pending("/in/Show.S01E01.mkv"),
            pending("/in/Show.S01E02.mkv"),
            pending("/in/Show.S03E01.mkv"),
        ];
        let episodes = fetch_episodes(&shows, "1", &files);
        assert_eq!(*shows.requested.borrow(), [Some(1), Some(3)]);
        assert_eq!(episodes.len(), 2);
    }

    #[test]
    fn date_based_shows_fetch_the_whole_episode_list() {
        let shows = StubShows::default();
        let files = [
            pending("/in/Show.2011.11.15.mkv"),
            pending("/in/Show.S01E01.mkv"),
        ];
        fetch_episodes(&shows, "1", &files);
        assert_eq!(*shows.requested.borrow(), [None]);
    }

    #[test]
    fn episode_fetch_failures_leave_an_empty_list() {
        let shows = StubShows {
            fail: true,
            ..StubShows::default()
        };
        let files = [pending("/in/Show.S01E01.mkv")];
        assert!(fetch_episodes(&shows, "1", &files).is_empty());
    }

    /// Two shows of the same name: a six-part miniseries and a long-running series.
    #[derive(Default)]
    struct NamesakeShows {
        listed: RefCell<Vec<String>>,
    }

    impl ShowSource for NamesakeShows {
        fn name(&self) -> &'static str {
            "tvdb"
        }

        fn search_shows(&self, _: &str, _: Option<i32>) -> Result<Vec<Candidate>> {
            Ok(vec![
                candidate("Archer", 1975, "mini"),
                candidate("Archer", 2009, "long"),
            ])
        }

        fn show(&self, id: &str) -> Result<Candidate> {
            Ok(candidate("Archer", 2009, id))
        }

        fn episodes(&self, id: &str, season: Option<u32>) -> Result<Vec<EpisodeInfo>> {
            assert_eq!(
                season, None,
                "candidates are checked against whole episode lists"
            );
            self.listed.borrow_mut().push(id.to_string());
            let seasons = if id == "mini" { 1 } else { 4 };
            let per_season = if id == "mini" { 6 } else { 13 };
            Ok((1..=seasons)
                .flat_map(|season| {
                    (1..=per_season).map(move |number| EpisodeInfo {
                        season,
                        number,
                        title: None,
                        air_date: None,
                        runtime: None,
                    })
                })
                .collect())
        }
    }

    fn show_group(paths: &[&str]) -> Group {
        group(paths.iter().map(|path| pending(path)).collect()).remove(0)
    }

    #[test]
    fn episode_lists_separate_shows_of_the_same_name() {
        let source = NamesakeShows::default();
        let group = show_group(&[
            "/in/Archer.S01E01.mkv",
            "/in/Archer.S02E03.mkv",
            "/in/Archer.S04E06.mkv",
        ]);
        let decision = decide(
            &group,
            &Lookup::Show(&source),
            &Library::in_memory().unwrap(),
            0.08,
            false,
            false,
            &mut None,
        )
        .unwrap();
        let Decision::Use(chosen) = decision else {
            panic!("expected a confident match");
        };
        assert_eq!(chosen.id, "long");
        assert_eq!(chosen.episodes.as_ref().map(Vec::len), Some(52));
        assert_eq!(*source.listed.borrow(), ["mini", "long"]);
    }

    fn quiet_decision(group: &Group, source: &dyn ShowSource) -> Decision {
        decide(
            group,
            &Lookup::Show(source),
            &Library::in_memory().unwrap(),
            0.08,
            false,
            false,
            &mut None,
        )
        .unwrap()
    }

    #[test]
    fn a_show_lacking_any_files_episode_is_not_matched_without_asking() {
        let source = NamesakeShows::default();
        let covered = show_group(&["/in/Archer.2009.S01E01.mkv", "/in/Archer.2009.S04E13.mkv"]);
        assert!(matches!(
            quiet_decision(&covered, &source),
            Decision::Use(chosen) if chosen.id == "long"
        ));

        let beyond = show_group(&["/in/Archer.2009.S01E01.mkv", "/in/Archer.2009.S05E01.mkv"]);
        assert!(matches!(quiet_decision(&beyond, &source), Decision::Skip));
    }

    #[test]
    fn an_id_tag_is_not_trusted_when_the_show_lacks_the_files_episodes() {
        let source = NamesakeShows::default();
        let tagged = show_group(&["/in/Archer {tvdb-long}/Archer.S01E01.mkv"]);
        assert!(matches!(
            quiet_decision(&tagged, &source),
            Decision::Use(chosen) if chosen.id == "long"
        ));

        let mistagged = show_group(&["/in/Archer {tvdb-mini}/Archer.S04E01.mkv"]);
        assert!(matches!(
            quiet_decision(&mistagged, &source),
            Decision::Skip
        ));
    }

    #[test]
    fn unmatched_files_are_red_only_when_colouring() {
        assert_eq!(unmatched_note(3, false), "3 file(s) unmatched");
        assert_eq!(
            unmatched_note(1, true),
            "\x1b[31m1 file(s) unmatched\x1b[0m"
        );
    }

    #[test]
    fn any_gap_in_an_episode_list_is_penalised() {
        let listed = |count: u32| -> Vec<EpisodeInfo> {
            (1..=count)
                .map(|number| EpisodeInfo {
                    season: 1,
                    number,
                    title: None,
                    air_date: None,
                    runtime: None,
                })
                .collect()
        };
        let files: Vec<Pending> = (1..=20)
            .map(|number| pending(&format!("/in/Show.S01E{number:02}.mkv")))
            .collect();
        assert_eq!(missing_episode_penalty(&files, &listed(20)), 0.0);
        assert!(
            missing_episode_penalty(&files, &listed(19)) >= 0.02,
            "a single unmatched file costs a visible amount"
        );
        assert_eq!(missing_episode_penalty(&files, &listed(0)), 0.52);
        assert!(missing_episode_penalty(&files, &listed(6)) > 0.3);

        let dated = [pending("/in/Show.2011.11.15.mkv")];
        let mut by_date = listed(1);
        assert_eq!(missing_episode_penalty(&dated, &by_date), 0.52);
        by_date[0].air_date = Some("2011-11-15".into());
        assert_eq!(missing_episode_penalty(&dated, &by_date), 0.0);
    }

    #[test]
    fn episodes_too_long_or_short_for_their_numbering_are_skipped() {
        let runs = |number, minutes| EpisodeInfo {
            season: 5,
            number,
            title: None,
            air_date: None,
            runtime: Some(minutes),
        };
        let listed = [runs(27, 10), runs(28, 45)];
        let place = |name: &str, seconds| {
            let parsed = parse::parse_name(name);
            let mut item = Item::new(Kind::Episode, "/in/file.mkv".into(), parsed.title.clone());
            apply_episode(&mut item, &parsed, &listed, Some(seconds))
        };
        assert!(place("Show S05E27", 670.0));
        assert!(!place("Show S05E28", 670.0), "a short is not the special");
        assert!(place("Show S05E28", 2710.0));
        assert!(
            !place("Show S05E27-E28", 670.0),
            "both episodes must fit in the file"
        );
        assert!(place("Show S05E27-E28", 3300.0));
    }

    #[test]
    fn an_unplaceable_episode_can_be_placed_by_hand() {
        let anthology = [
            EpisodeInfo {
                season: 13,
                number: 4,
                title: Some("Winter Wedding".into()),
                air_date: Some("2005-02-27".into()),
                runtime: Some(90),
            },
            EpisodeInfo {
                season: 13,
                number: 5,
                title: Some("Summer Solstice".into()),
                air_date: Some("2005-03-27".into()),
                runtime: Some(90),
            },
        ];
        let parsed = parse::parse_name("Summer Solstice (2006) - s01e01");
        let attempt = |answers: &[Option<&str>]| {
            let answers = RefCell::new(
                answers
                    .iter()
                    .map(|a| a.map(String::from))
                    .collect::<Vec<_>>(),
            );
            let ask = |_: &str| Ok(answers.borrow_mut().remove(0));
            let mut item = Item::new(
                Kind::Episode,
                "/in/s.mkv".into(),
                "Rosamunde Pilcher".into(),
            );
            let placement =
                place_episode(&mut item, &parsed, &anthology, Some(5400.0), Some(ask)).unwrap();
            (placement, item)
        };
        let place = |answers: &[&str]| {
            let answers: Vec<_> = answers.iter().copied().map(Some).collect();
            let (placement, item) = attempt(&answers);
            (placement == Placement::Placed).then_some(item)
        };

        let item = place(&["solstice", "s13e05"]).expect("placed by number after a search");
        assert_eq!((item.season, item.episode), (Some(13), Some(5)));
        assert_eq!(item.episode_title.as_deref(), Some("Summer Solstice"));

        let item = place(&["2005-02-27"]).expect("placed by air date");
        assert_eq!((item.season, item.episode), (Some(13), Some(4)));

        assert!(
            place(&["s09e09", ""]).is_none(),
            "unlisted answers ask again"
        );
        assert_eq!(attempt(&[Some("")]).0, Placement::Skipped);
        assert_eq!(
            attempt(&[Some("solstice"), None]).0,
            Placement::Aborted,
            "Ctrl-C stops importing rather than skipping"
        );

        let mut item = Item::new(
            Kind::Episode,
            "/in/s.mkv".into(),
            "Rosamunde Pilcher".into(),
        );
        let never = None::<fn(&str) -> Result<Option<String>>>;
        assert_eq!(
            place_episode(&mut item, &parsed, &anthology, None, never).unwrap(),
            Placement::Skipped
        );
    }

    #[test]
    fn unlisted_episodes_are_skipped() {
        assert!(applied("Show S05E09 Parsed Title", &episodes()).is_none());
        assert!(
            applied("Show S01E02-E03", &episodes()).is_none(),
            "every episode a file spans must be listed"
        );
    }

    #[test]
    fn files_without_episode_numbers_cannot_be_placed() {
        assert!(applied("Show Special Feature", &episodes()).is_none());
    }

    fn write(dir: &Path, relative: &str, contents: &str) -> PathBuf {
        let path = dir.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn collection_finds_videos_and_skips_samples_extras_and_other_files() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "Film/Film.2000.mkv",
            "Film/Film.2000.en.srt",
            "Film/Film.nfo",
            "Film/Sample/film-sample.mkv",
            "Film/Featurettes/making-of.mkv",
            "More/Thing.2001.MP4",
        ] {
            write(dir.path(), name, "");
        }
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let found = collect(&[dir.path().to_path_buf()]).unwrap();
        let names: Vec<_> = found
            .iter()
            .map(|(path, _)| path.strip_prefix(&root).unwrap().to_str().unwrap())
            .collect();
        assert_eq!(names, ["Film/Film.2000.mkv", "More/Thing.2001.MP4"]);
        assert!(found.iter().all(|(_, found_root)| *found_root == root));
    }

    #[test]
    fn collecting_a_single_file_uses_its_directory_as_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let file = write(dir.path(), "extras/sample.mkv", "");
        let found = collect(std::slice::from_ref(&file)).unwrap();
        let canonical = std::fs::canonicalize(&file).unwrap();
        assert_eq!(
            found,
            [(canonical.clone(), canonical.parent().unwrap().to_path_buf())],
            "a file named explicitly is imported even if it looks like a sample"
        );
    }

    #[test]
    fn collecting_a_missing_path_is_an_error() {
        let error = collect(&[PathBuf::from("/definitely/not/here")]).unwrap_err();
        assert!(error.to_string().contains("cannot access"));
    }

    /// A metadata service that knows every title it is asked about, exactly as it was asked,
    /// numbering each new one from 1.
    #[derive(Default)]
    struct Echo {
        known: RefCell<Vec<Candidate>>,
        /// The episode list every show has.
        episodes: Vec<EpisodeInfo>,
    }

    impl Echo {
        fn search(&self, source: &'static str, title: &str, year: Option<i32>) -> Vec<Candidate> {
            let mut known = self.known.borrow_mut();
            let existing = known
                .iter()
                .find(|work| work.source == source && work.title == title && work.year == year);
            let work = match existing {
                Some(work) => work.clone(),
                None => {
                    let work = Candidate {
                        source,
                        id: (known.len() + 1).to_string(),
                        title: title.into(),
                        year,
                        url: String::new(),
                        original_title: None,
                        overview: None,
                        runtime: None,
                        episodes: None,
                    };
                    known.push(work.clone());
                    work
                }
            };
            vec![work]
        }

        fn by_id(&self, source: &str, id: &str) -> Result<Candidate> {
            self.known
                .borrow()
                .iter()
                .find(|work| work.source == source && work.id == id)
                .cloned()
                .ok_or_else(|| anyhow!("no {source}-{id}"))
        }
    }

    impl MovieSource for Echo {
        fn search_movies(&self, title: &str, year: Option<i32>) -> Result<Vec<Candidate>> {
            Ok(self.search("tmdb", title, year))
        }

        fn movie(&self, id: &str) -> Result<Candidate> {
            self.by_id("tmdb", id)
        }
    }

    impl ShowSource for Echo {
        fn name(&self) -> &'static str {
            "tvdb"
        }

        fn search_shows(&self, title: &str, year: Option<i32>) -> Result<Vec<Candidate>> {
            Ok(self.search("tvdb", title, year))
        }

        fn show(&self, id: &str) -> Result<Candidate> {
            self.by_id("tvdb", id)
        }

        fn episodes(&self, _: &str, season: Option<u32>) -> Result<Vec<EpisodeInfo>> {
            Ok(self
                .episodes
                .iter()
                .filter(|episode| season.is_none_or(|season| episode.season == season))
                .cloned()
                .collect())
        }
    }

    impl Sources for Echo {
        fn movies(&mut self) -> Result<&dyn MovieSource> {
            Ok(self)
        }

        fn shows(&mut self) -> Result<&dyn ShowSource> {
            Ok(self)
        }
    }

    struct Env {
        dir: tempfile::TempDir,
        config: Config,
        library: Library,
        sources: Echo,
    }

    impl Env {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let mut config = Config::default();
            config.library.movies = dir.path().join("Movies");
            config.library.tv = dir.path().join("TV");
            config.import.probe = false;
            Env {
                dir,
                config,
                library: Library::in_memory().unwrap(),
                sources: Echo::default(),
            }
        }

        fn write(&self, relative: &str, contents: &str) -> PathBuf {
            write(self.dir.path(), relative, contents)
        }

        fn path(&self, relative: &str) -> PathBuf {
            self.dir.path().join(relative)
        }

        /// Imports, answering any question about versions with `choice`, and returns how many
        /// library files each question showed.
        fn import_choosing(&mut self, options: &Options, choice: Choice) -> Result<Vec<usize>> {
            let shown = std::cell::RefCell::new(Vec::new());
            let resolve = |_: &Item, _: &Path, existing: &[Item]| {
                shown.borrow_mut().push(existing.len());
                Ok(choice)
            };
            run_with(
                &self.config,
                &mut self.library,
                options,
                false,
                &mut self.sources,
                Some(&resolve),
            )?;
            Ok(shown.into_inner())
        }

        fn import(&mut self, options: &Options) -> Result<()> {
            run_with(
                &self.config,
                &mut self.library,
                options,
                false,
                &mut self.sources,
                None,
            )
        }

        fn rescan(&mut self, paths: &[&str], pretend: bool) -> Result<()> {
            let rescan = Rescan {
                paths: paths.iter().map(|path| self.path(path)).collect(),
                quiet: true,
                pretend,
                timid: false,
                no_timid: false,
                skip_length_check: false,
            };
            let options = rescan_options(&self.config, rescan)?;
            run_with(
                &self.config,
                &mut self.library,
                &options,
                true,
                &mut self.sources,
                None,
            )
        }

        fn rows(&self) -> Vec<Item> {
            self.library.query(&crate::query::Query::default()).unwrap()
        }

        fn paths(&self) -> Vec<PathBuf> {
            let mut paths: Vec<_> = self.rows().into_iter().map(|item| item.path).collect();
            paths.sort();
            paths
        }
    }

    fn options(env: &Env, paths: &[&str]) -> Options {
        Options {
            paths: paths.iter().map(|path| env.path(path)).collect(),
            operation: None,
            remux_args: Vec::new(),
            quiet: true,
            pretend: false,
            kind: None,
            timid: false,
            no_timid: false,
            skip_length_check: false,
        }
    }

    #[test]
    fn importing_copies_a_film_into_the_library() {
        let mut env = Env::new();
        let source = env.write("in/Heat.1995.1080p.BluRay.mkv", "video");
        env.import(&options(&env, &["in"])).unwrap();

        let destination = env.path("Movies/Heat (1995) {tmdb-1}/Heat (1995) {tmdb-1}.mkv");
        assert_eq!(std::fs::read_to_string(&destination).unwrap(), "video");
        assert!(source.exists(), "copying leaves the source");

        let rows = env.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, destination);
        assert_eq!((rows[0].title.as_str(), rows[0].year), ("Heat", Some(1995)));
        assert_eq!(rows[0].media.resolution.as_deref(), Some("1080p"));
        assert_eq!(rows[0].size, 5);
        assert_eq!(
            rows[0].original_path.as_deref(),
            Some(std::fs::canonicalize(&source).unwrap().as_path())
        );
        assert!(rows[0].added > 0);
    }

    #[test]
    fn importing_an_episode_files_it_by_season() {
        let mut env = Env::new();
        env.write("in/Show.Name.S02E03.Some.Title.720p.mkv", "video");
        env.sources.episodes = vec![EpisodeInfo {
            season: 2,
            number: 3,
            title: Some("Some Title".into()),
            air_date: None,
            runtime: None,
        }];
        env.import(&options(&env, &["in"])).unwrap();

        let rows = env.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, Kind::Episode);
        assert_eq!((rows[0].season, rows[0].episode), (Some(2), Some(3)));
        assert_eq!(
            rows[0].path,
            env.path("TV/Show Name {tvdb-1}/Season 02/Show Name - s02e03 - Some Title.mkv")
        );
        assert!(rows[0].path.exists());
    }

    #[test]
    fn the_operation_option_overrides_the_configuration_and_prunes_directories() {
        let mut env = Env::new();
        let source = env.write("in/Heat 1995/heat.1995.mkv", "video");
        let mut options = options(&env, &["in"]);
        options.operation = Some(Operation::Move);
        env.import(&options).unwrap();

        assert!(!source.exists());
        assert!(
            !env.path("in/Heat 1995").exists(),
            "the emptied directory goes"
        );
        assert!(
            env.path("in").is_dir(),
            "the directory given on the command line stays"
        );
        assert!(env.rows()[0].path.exists());
    }

    /// Points the configuration at a stand-in videoconverter that runs `body` with the output
    /// path in `$out`.
    #[cfg(unix)]
    fn fake_videoconverter(env: &mut Env, body: &str) {
        let program = env.path("videoconverter");
        remux::write_script(
            &program,
            &format!(
                "#!/bin/sh\n[ \"$1\" = --version ] && echo videoconverter 0.5.0 && exit\n\
                 for arg; do [ \"$prev\" = --output ] && out=$arg && {{ {body}; }}; prev=$arg; done\n"
            ),
        );
        env.config.import.videoconverter = program.to_string_lossy().into_owned();
        env.config.import.operation = Operation::Remux;
    }

    #[cfg(unix)]
    #[test]
    fn remuxing_writes_matroska_and_leaves_the_source() {
        let mut env = Env::new();
        fake_videoconverter(&mut env, "echo remuxed > \"$out\"");
        let source = env.write("in/Heat.1995.mp4", "video");
        env.write("in/Heat.1995.en.srt", "subs");
        env.import(&options(&env, &["in"])).unwrap();

        let destination = env.path("Movies/Heat (1995) {tmdb-1}/Heat (1995) {tmdb-1}.mkv");
        assert_eq!(std::fs::read_to_string(&destination).unwrap(), "remuxed\n");
        assert!(destination.with_extension("en.srt").exists());
        assert!(source.exists(), "remuxing leaves the source");
        let rows = env.rows();
        assert_eq!(rows[0].path, destination);
        assert_eq!(rows[0].size, 8, "the stamp is the remuxed file's");
    }

    #[cfg(unix)]
    #[test]
    fn an_old_videoconverter_stops_the_import_before_anything_happens() {
        let mut env = Env::new();
        fake_videoconverter(&mut env, "touch \"$out\"");
        let program = env.path("old");
        remux::write_script(&program, "#!/bin/sh\necho videoconverter 0.4.0\n");
        env.config.import.videoconverter = program.to_string_lossy().into_owned();
        env.write("in/Heat.1995.mkv", "video");

        let err = env.import(&options(&env, &["in"])).unwrap_err();
        assert!(format!("{err:#}").contains("0.5 or later"));
        assert!(env.rows().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_remux_imports_nothing() {
        let mut env = Env::new();
        fake_videoconverter(&mut env, "exit 1");
        env.write("in/Heat.1995.mkv", "video");
        env.import(&options(&env, &["in"])).unwrap();

        assert!(env.rows().is_empty());
        assert!(!env.path("Movies/Heat (1995) {tmdb-1}").exists());
    }

    /// Imports a 1080p Heat with a sidecar, then offers a 2160p one, answering with `choice`.
    fn offer_a_second_heat(env: &mut Env, choice: Choice) -> Vec<usize> {
        env.write("old/Heat.1995.1080p.mkv", "old video");
        env.write("old/Heat.1995.1080p.en.srt", "old subs");
        env.import(&options(env, &["old"])).unwrap();
        env.write("new/Heat.1995.2160p.mkv", "new, bigger video");
        env.import_choosing(&options(env, &["new"]), choice).unwrap()
    }

    const HEAT: &str = "Movies/Heat (1995) {tmdb-1}/Heat (1995) {tmdb-1}.mkv";

    #[test]
    fn a_new_version_can_replace_the_old_one() {
        let mut env = Env::new();
        assert_eq!(offer_a_second_heat(&mut env, Choice::Replace(0)), [1]);
        let rows = env.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, env.path(HEAT), "no version suffix is needed any more");
        assert_eq!(rows[0].media.resolution.as_deref(), Some("2160p"));
        assert_eq!(std::fs::read_to_string(env.path(HEAT)).unwrap(), "new, bigger video");
        let folder = env.path(HEAT).parent().unwrap().to_owned();
        let left: Vec<_> = std::fs::read_dir(&folder)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(left, [std::ffi::OsString::from("Heat (1995) {tmdb-1}.mkv")], "the old sidecar goes too");
    }

    #[test]
    fn a_replacement_brings_its_own_sidecars() {
        let mut env = Env::new();
        env.write("new/Heat.1995.2160p.en.srt", "new subs");
        offer_a_second_heat(&mut env, Choice::Replace(0));
        let sidecar = env.path(HEAT).with_extension("en.srt");
        assert_eq!(std::fs::read_to_string(sidecar).unwrap(), "new subs");
    }

    #[test]
    fn keeping_both_imports_another_version() {
        let mut env = Env::new();
        offer_a_second_heat(&mut env, Choice::KeepBoth);
        assert_eq!(env.rows().len(), 2);
        assert_eq!(std::fs::read_to_string(env.path(HEAT)).unwrap(), "old video");
    }

    #[test]
    fn skipping_leaves_the_library_alone() {
        let mut env = Env::new();
        offer_a_second_heat(&mut env, Choice::Skip);
        let rows = env.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].media.resolution.as_deref(), Some("1080p"));
        assert!(env.path(HEAT).with_extension("en.srt").exists());
    }

    #[test]
    fn stopping_at_the_question_imports_nothing_more() {
        let mut env = Env::new();
        env.write("new/Ronin.1998.mkv", "another film");
        offer_a_second_heat(&mut env, Choice::Abort);
        assert_eq!(env.rows().len(), 1);
    }

    #[test]
    fn exact_copies_are_skipped_without_asking() {
        let mut env = Env::new();
        env.write("old/Heat.1995.1080p.mkv", "video");
        env.import(&options(&env, &["old"])).unwrap();
        env.write("new/Heat.1995.1080p.mkv", "video");
        let shown = env.import_choosing(&options(&env, &["new"]), Choice::KeepBoth).unwrap();
        assert!(shown.is_empty());
        assert_eq!(env.rows().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_remux_replaces_the_old_file_once_it_has_finished() {
        let mut env = Env::new();
        env.write("old/Heat.1995.1080p.mkv", "old video");
        env.import(&options(&env, &["old"])).unwrap();
        fake_videoconverter(&mut env, "echo remuxed > \"$out\"");
        env.write("new/Heat.1995.2160p.mp4", "new video");
        env.import_choosing(&options(&env, &["new"]), Choice::Replace(0)).unwrap();
        let rows = env.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, env.path(HEAT));
        assert_eq!(std::fs::read_to_string(env.path(HEAT)).unwrap(), "remuxed\n");
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_remux_keeps_the_file_it_would_have_replaced() {
        let mut env = Env::new();
        env.write("old/Heat.1995.1080p.mkv", "old video");
        env.import(&options(&env, &["old"])).unwrap();
        fake_videoconverter(&mut env, "exit 1");
        env.write("new/Heat.1995.2160p.mp4", "new video");
        env.import_choosing(&options(&env, &["new"]), Choice::Replace(0)).unwrap();
        assert_eq!(env.rows().len(), 1);
        assert_eq!(std::fs::read_to_string(env.path(HEAT)).unwrap(), "old video");
    }

    #[test]
    fn files_already_imported_from_are_skipped() {
        let mut env = Env::new();
        env.write("in/Heat.1995.mkv", "video");
        env.import(&options(&env, &["in"])).unwrap();
        let row = &env.rows()[0];
        // As after a remux, the library's file and the source no longer share a size.
        std::fs::write(&row.path, "a different size").unwrap();
        let mut item = row.clone();
        (item.size, item.mtime) = file_stamp(&row.path).unwrap();
        env.library.save(&mut item).unwrap();
        env.import(&options(&env, &["in"])).unwrap();
        assert_eq!(env.rows().len(), 1);
    }

    #[test]
    fn a_new_file_where_an_imported_one_was_is_imported() {
        let mut env = Env::new();
        let source = env.write("in/Heat.1995.mkv", "video");
        let mut options = options(&env, &["in"]);
        options.operation = Some(Operation::Move);
        env.import(&options).unwrap();
        assert!(!source.exists());

        env.write("in/Heat.1995.mkv", "a re-download");
        env.import(&options).unwrap();
        assert_eq!(env.rows().len(), 2, "both versions are in the library");
    }

    #[test]
    fn the_configured_operation_applies_by_default() {
        let mut env = Env::new();
        env.config.import.operation = Operation::Move;
        let source = env.write("in/Heat.1995.mkv", "video");
        env.import(&options(&env, &["in"])).unwrap();
        assert!(!source.exists());
    }

    #[test]
    fn pretending_touches_neither_files_nor_the_database() {
        let mut env = Env::new();
        let source = env.write("in/Heat.1995.mkv", "video");
        let mut options = options(&env, &["in"]);
        options.pretend = true;
        env.import(&options).unwrap();

        assert!(source.exists());
        assert!(env.rows().is_empty());
        assert!(!env.path("Movies").exists());
    }

    #[test]
    fn subtitles_follow_their_video_when_enabled() {
        let mut env = Env::new();
        env.write("in/Heat.1995.mkv", "video");
        env.write("in/Heat.1995.en.forced.srt", "sub");
        env.write("in/Heat.1995.fr.srt", "sub");
        env.import(&options(&env, &["in"])).unwrap();

        let folder = env.path("Movies/Heat (1995) {tmdb-1}");
        assert!(folder.join("Heat (1995) {tmdb-1}.en.forced.srt").exists());
        assert!(folder.join("Heat (1995) {tmdb-1}.fr.srt").exists());
    }

    #[test]
    fn subtitles_stay_behind_when_disabled() {
        let mut env = Env::new();
        env.config.import.subtitles = false;
        env.write("in/Heat.1995.mkv", "video");
        env.write("in/Heat.1995.en.srt", "sub");
        env.import(&options(&env, &["in"])).unwrap();
        assert!(
            !env.path("Movies/Heat (1995) {tmdb-1}/Heat (1995) {tmdb-1}.en.srt")
                .exists()
        );
    }

    #[test]
    fn importing_twice_does_not_duplicate_files() {
        let mut env = Env::new();
        env.write("in/Heat.1995.1080p.mkv", "video");
        env.import(&options(&env, &["in"])).unwrap();
        env.import(&options(&env, &["in"])).unwrap();

        assert_eq!(env.rows().len(), 1);
        let folder: Vec<_> = std::fs::read_dir(env.path("Movies/Heat (1995) {tmdb-1}"))
            .unwrap()
            .collect();
        assert_eq!(folder.len(), 1);
    }

    #[test]
    fn identical_files_in_one_run_are_imported_once() {
        let mut env = Env::new();
        env.write("in/a/Heat.1995.1080p.mkv", "video");
        env.write("in/b/Heat.1995.1080p.mkv", "video");
        env.import(&options(&env, &["in"])).unwrap();
        assert_eq!(env.rows().len(), 1);
    }

    #[test]
    fn a_second_version_gets_a_resolution_suffix_and_move_tidies_the_first() {
        let mut env = Env::new();
        env.write("in/Heat.1995.1080p.mkv", "video");
        env.import(&options(&env, &["in"])).unwrap();
        assert_eq!(
            env.paths(),
            [env.path("Movies/Heat (1995) {tmdb-1}/Heat (1995) {tmdb-1}.mkv")]
        );

        env.write("in2/Heat.1995.2160p.mkv", "better video");
        env.import(&options(&env, &["in2"])).unwrap();
        assert_eq!(env.rows().len(), 2);
        assert!(
            env.path("Movies/Heat (1995) {tmdb-1}/Heat (1995) {tmdb-1} - 2160p.mkv")
                .exists()
        );
        assert!(
            env.path("Movies/Heat (1995) {tmdb-1}/Heat (1995) {tmdb-1}.mkv")
                .exists()
        );

        crate::commands::move_items(
            &env.config,
            &mut env.library,
            &crate::query::Query::default(),
            false,
        )
        .unwrap();
        assert_eq!(
            env.paths(),
            [
                env.path("Movies/Heat (1995) {tmdb-1}/Heat (1995) {tmdb-1} - 1080p.mkv"),
                env.path("Movies/Heat (1995) {tmdb-1}/Heat (1995) {tmdb-1} - 2160p.mkv"),
            ]
        );
    }

    #[test]
    fn existing_unregistered_files_are_never_overwritten() {
        let mut env = Env::new();
        let occupied = env.write(
            "Movies/Heat (1995) {tmdb-1}/Heat (1995) {tmdb-1}.mkv",
            "precious",
        );
        env.write("in/Heat.1995.mkv", "newcomer");
        env.import(&options(&env, &["in"])).unwrap();

        assert_eq!(std::fs::read_to_string(&occupied).unwrap(), "precious");
        assert!(env.rows().is_empty());
    }

    #[test]
    fn the_kind_option_overrides_guessing() {
        let mut env = Env::new();
        env.write("in/Show.S01E01.mkv", "video");
        let mut as_film = options(&env, &["in"]);
        as_film.kind = Some(Kind::Movie);
        env.import(&as_film).unwrap();
        let rows = env.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, Kind::Movie);
        assert_eq!((rows[0].season, rows[0].episode), (None, None));
    }

    #[test]
    fn forcing_episodes_skips_files_without_numbers() {
        let mut env = Env::new();
        env.write("in/Heat.1995.mkv", "video");
        let mut as_episode = options(&env, &["in"]);
        as_episode.kind = Some(Kind::Episode);
        env.import(&as_episode).unwrap();
        assert!(env.rows().is_empty());
    }

    #[test]
    fn importing_a_missing_path_fails() {
        let mut env = Env::new();
        let error = env.import(&options(&env, &["nowhere"])).unwrap_err();
        assert!(error.to_string().contains("cannot access"));
    }

    #[test]
    fn importing_without_videos_is_a_no_op() {
        let mut env = Env::new();
        env.write("in/readme.txt", "hello");
        env.import(&options(&env, &["in"])).unwrap();
        assert!(env.rows().is_empty());
    }

    #[test]
    fn rescanning_records_files_where_they_are() {
        let mut env = Env::new();
        let film = env.write("Movies/messy/Heat.1995.mkv", "video");
        let episode = env.write("TV/Show/Show.S01E01.mkv", "video");
        env.sources.episodes = vec![EpisodeInfo {
            season: 1,
            number: 1,
            title: None,
            air_date: None,
            runtime: None,
        }];
        env.rescan(&[], false).unwrap();

        let mut expected = vec![film.clone(), episode.clone()];
        expected.sort();
        assert_eq!(env.paths(), expected);
        assert!(film.exists() && episode.exists(), "nothing moves");
        assert!(env.rows().iter().all(|item| item.original_path.is_none()));
    }

    #[test]
    fn rescanning_twice_adds_nothing_new() {
        let mut env = Env::new();
        env.write("Movies/Heat.1995.mkv", "video");
        env.rescan(&[], false).unwrap();
        let first = env.rows();
        env.rescan(&[], false).unwrap();
        assert_eq!(env.rows().len(), 1);
        assert_eq!(env.rows()[0].id, first[0].id);
    }

    #[test]
    fn a_pretend_rescan_records_nothing() {
        let mut env = Env::new();
        env.write("Movies/Heat.1995.mkv", "video");
        env.rescan(&[], true).unwrap();
        assert!(env.rows().is_empty());
    }

    #[test]
    fn rescanning_given_paths_ignores_the_rest_of_the_library() {
        let mut env = Env::new();
        env.write("Movies/Heat.1995.mkv", "video");
        env.write("Movies/Ronin.1998.mkv", "video");
        env.write("TV/Show/Show.S01E01.mkv", "video");
        env.rescan(&["Movies/Ronin.1998.mkv"], false).unwrap();
        assert_eq!(env.paths(), [env.path("Movies/Ronin.1998.mkv")]);
    }

    #[test]
    fn rescanning_tolerates_one_missing_library_folder() {
        let mut env = Env::new();
        env.write("Movies/Heat.1995.mkv", "video");
        env.rescan(&[], false).unwrap();
        assert_eq!(env.rows().len(), 1);
    }

    #[test]
    fn rescanning_fails_when_no_library_folder_exists() {
        let mut env = Env::new();
        let error = env.rescan(&[], false).unwrap_err();
        assert!(error.to_string().contains("neither"));
    }

    #[test]
    fn a_rescanned_library_can_be_renamed_afterwards() {
        let mut env = Env::new();
        env.write("Movies/messy/Heat.1995.mkv", "video");
        env.rescan(&[], false).unwrap();
        crate::commands::move_items(
            &env.config,
            &mut env.library,
            &crate::query::Query::default(),
            false,
        )
        .unwrap();
        assert_eq!(
            env.paths(),
            [env.path("Movies/Heat (1995) {tmdb-1}/Heat (1995) {tmdb-1}.mkv")]
        );
        assert!(!env.path("Movies/messy").exists());
    }
}
