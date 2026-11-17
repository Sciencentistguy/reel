//! The import pipeline: discover files, identify them, then file them into the library.

use std::collections::{BTreeSet, HashMap};
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rustyline::error::ReadlineError;
use walkdir::WalkDir;

use crate::config::{Config, Operation};
use crate::fileops;
use crate::library::Library;
use crate::metadata::{self, Candidate, EpisodeInfo, MovieSource, ShowSource};
use crate::model::{Item, Kind};
use crate::naming;
use crate::parse::{self, Parsed};
use crate::probe;

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
    /// Never prompt; skip anything without a confident match.
    #[arg(short, long)]
    pub quiet: bool,
    /// Show what would happen without touching files or the database.
    #[arg(short = 'n', long)]
    pub pretend: bool,
    /// Treat every file as this kind instead of guessing from its name.
    #[arg(short, long, value_enum)]
    pub kind: Option<Kind>,
    /// Skip metadata lookups and name files from what their current names say.
    #[arg(short = 'A', long)]
    pub asis: bool,
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
    files: Vec<Pending>,
}

enum Decision {
    Use(Candidate),
    AsIs,
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
    if ignored > 0 {
        println!("Ignoring {ignored} sample or extras file(s).");
    }
    Ok(files)
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

fn match_percent(distance: f64) -> String {
    let colour = io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    colour_percent(((1.0 - distance) * 100.0).round(), colour)
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

fn decide(
    group: &Group,
    lookup: &Lookup,
    library: &Library,
    threshold: f64,
    interactive: bool,
) -> Result<Decision> {
    if let Some((source, id)) = &group.id_hint
        && source == lookup.source()
    {
        match lookup.by_id(id) {
            Ok(candidate) => {
                println!(
                    "  Matched by ID: {} [{source}-{id}]",
                    describe(&candidate.title, candidate.year)
                );
                return Ok(Decision::Use(candidate));
            }
            Err(err) => eprintln!("  warning: lookup of {source}-{id} failed: {err:#}"),
        }
    }

    let (mut title, mut year) = (group.title.clone(), group.year);
    let mut searched_manually = false;
    loop {
        let ranked = metadata::rank(&title, year, lookup.search(&title, year)?);
        if !searched_manually && metadata::is_strong(&ranked, threshold) {
            let (distance, candidate) = ranked.into_iter().next().unwrap();
            println!(
                "  Matched: {} [{}-{}] ({})",
                describe(&candidate.title, candidate.year),
                candidate.source,
                candidate.id,
                match_percent(distance)
            );
            return Ok(Decision::Use(candidate));
        }
        if !interactive {
            println!("  No confident match; skipping.");
            return Ok(Decision::Skip);
        }

        let shown = &ranked[..ranked.len().min(8)];
        if shown.is_empty() {
            println!("  No candidates found for {:?}.", describe(&title, year));
        }
        let in_library = shown
            .iter()
            .map(|(_, candidate)| {
                library.count_identified(group.kind, candidate.source, &candidate.id)
            })
            .collect::<Result<Vec<_>>>()?;
        for (number, ((distance, candidate), count)) in shown.iter().zip(&in_library).enumerate() {
            let known = match count {
                0 => String::new(),
                count => format!(" - already in library ({count} file(s))"),
            };
            println!(
                "  {}. {} [{}-{}] ({}){known}",
                number + 1,
                describe(&candidate.title, candidate.year),
                candidate.source,
                candidate.id,
                match_percent(*distance)
            );
        }
        loop {
            let default = if shown.is_empty() {
                "s".to_string()
            } else {
                let first_known = in_library.iter().position(|&count| count > 0);
                (first_known.unwrap_or(0) + 1).to_string()
            };
            let Some(answer) = prompt(&format!(
                "  # selection, [s]kip, [u]se as-is, [e]nter search, enter [i]d, a[b]ort? [{default}] "
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
                "u" => return Ok(Decision::AsIs),
                "b" => return Ok(Decision::Abort),
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
                "i" => {
                    let Some(id) = prompt(&format!("  {} ID: ", lookup.source()))? else {
                        return Ok(Decision::Abort);
                    };
                    match lookup.by_id(&id) {
                        Ok(candidate) => return Ok(Decision::Use(candidate)),
                        Err(err) => eprintln!("  {err:#}"),
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

fn base_item(group: &Group, file: &Pending, candidate: Option<&Candidate>) -> Item {
    let mut item = Item::new(group.kind, file.path.clone(), group.title.clone());
    item.year = group.year;
    if let Some(candidate) = candidate {
        item.title = candidate.title.clone();
        item.year = candidate.year.or(group.year);
        item.source = Some(candidate.source.to_string());
        item.source_id = Some(candidate.id.clone());
    }
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

/// Fills in the episode fields of `item`. Returns `false` when the file cannot be placed.
fn apply_episode(item: &mut Item, parsed: &Parsed, episodes: Option<&[EpisodeInfo]>) -> bool {
    item.season = parsed.season;
    item.episode = parsed.episode;
    item.episode_end = parsed.episode_end;
    item.air_date = parsed.air_date.clone();
    item.date_based = parsed.air_date.is_some();
    let Some(episodes) = episodes else {
        item.episode_title = parsed.episode_title.clone();
        return true;
    };

    if let Some(date) = &parsed.air_date {
        let Some(found) = episodes.iter().find(|e| e.air_date.as_ref() == Some(date)) else {
            eprintln!(
                "  warning: no episode aired on {date}; skipping {}",
                item.path.display()
            );
            return false;
        };
        item.season = Some(found.season);
        item.episode = Some(found.number);
        item.episode_title = found.title.clone();
        return true;
    }

    let (Some(season), Some(first)) = (parsed.season, parsed.episode) else {
        return false;
    };
    let last = parsed.episode_end.unwrap_or(first);
    let mut titles: Vec<&str> = Vec::new();
    for number in first..=last {
        match episodes
            .iter()
            .find(|e| e.season == season && e.number == number)
        {
            Some(found) => {
                if let Some(title) = found.title.as_deref()
                    && !titles.contains(&title)
                {
                    titles.push(title);
                }
                if number == first {
                    item.air_date = found.air_date.clone();
                }
            }
            None => eprintln!(
                "  warning: s{season:02}e{number:02} is not in the metadata; importing without a title"
            ),
        }
    }
    item.episode_title = Some(titles.join(" & ")).filter(|t| !t.is_empty());
    true
}

struct Importer<'a> {
    config: &'a Config,
    library: &'a mut Library,
    operation: Operation,
    pretend: bool,
    probe: bool,
    /// What a pretend run would have added, so later files still see earlier ones as versions.
    planned: Vec<Item>,
    versioned: bool,
    summary: Summary,
}

impl Importer<'_> {
    /// Returns whether the file was imported (as opposed to skipped).
    fn import(&mut self, mut item: Item, file: &Pending) -> Result<bool> {
        if self.probe {
            match probe::probe(&file.path) {
                Ok(media) => item.media = media,
                Err(err) => {
                    eprintln!("  warning: {err:#}");
                    if err.downcast_ref::<io::Error>().is_some() {
                        eprintln!("  warning: continuing without media information");
                        self.probe = false;
                    }
                }
            }
        }
        if item.media.resolution.is_none() {
            item.media.resolution = file.parsed.resolution.clone();
        }
        (item.size, item.mtime) = file_stamp(&file.path)?;

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
            return Ok(false);
        }

        let has_versions = self.library.has_other_versions(&item)?
            || self.planned.iter().any(|planned| planned.same_work(&item));
        let version = if has_versions {
            item.media.resolution.clone()
        } else {
            None
        };
        let destination = naming::destination(self.config, &item, version.as_deref());
        let in_place = destination == file.path;
        if !in_place && destination.symlink_metadata().is_ok() {
            eprintln!(
                "  warning: {} already exists; skipping {}",
                destination.display(),
                file.path.display()
            );
            return Ok(false);
        }

        let name = file.path.file_name().unwrap_or_default().to_string_lossy();
        if in_place {
            println!("  {name} (already in place)");
        } else {
            println!("  {name}\n    -> {}", destination.display());
        }
        self.versioned |= version.is_some();
        if self.pretend {
            self.planned.push(item);
            return Ok(true);
        }

        if !in_place {
            let sidecars = if self.config.import.subtitles {
                naming::sidecars(&file.path)
            } else {
                Vec::new()
            };
            fileops::transfer(&file.path, &destination, self.operation)?;
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
        item.original_path = Some(file.path.clone());
        item.path = destination;
        item.added = now();
        self.library.save(&mut item)?;
        Ok(true)
    }
}

pub fn run(config: &Config, library: &mut Library, options: &Options) -> Result<()> {
    let mut pending = Vec::new();
    let mut known = 0;
    for (path, root) in collect(&options.paths)? {
        if library.contains_path(&path)? {
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

    let interactive = !options.quiet && io::stdin().is_terminal();
    let mut movies: Option<Box<dyn MovieSource>> = None;
    let mut shows: Option<Box<dyn ShowSource>> = None;
    let mut importer = Importer {
        config,
        library,
        operation: options.operation.unwrap_or(config.import.operation),
        pretend: options.pretend,
        probe: config.import.probe,
        planned: Vec::new(),
        versioned: false,
        summary: Summary::default(),
    };

    for group in group(pending) {
        let label = match group.kind {
            Kind::Movie => "film",
            Kind::Episode => "show",
        };
        println!(
            "\n{} ({label}, {} file(s))",
            describe(&group.title, group.year),
            group.files.len()
        );

        let decision = if options.asis {
            Decision::AsIs
        } else {
            let lookup = match group.kind {
                Kind::Movie => {
                    if movies.is_none() {
                        movies = Some(metadata::movie_source(&config.metadata)?);
                    }
                    Lookup::Movie(movies.as_deref().unwrap())
                }
                Kind::Episode => {
                    if shows.is_none() {
                        shows = Some(metadata::show_source(&config.metadata)?);
                    }
                    Lookup::Show(shows.as_deref().unwrap())
                }
            };
            decide(
                &group,
                &lookup,
                importer.library,
                config.import.strong_match,
                interactive,
            )?
        };
        let candidate = match decision {
            Decision::Use(candidate) => Some(candidate),
            Decision::AsIs => None,
            Decision::Skip => {
                importer.summary.skipped += group.files.len();
                continue;
            }
            Decision::Abort => break,
        };

        let episodes = match (&candidate, group.kind, shows.as_deref()) {
            (Some(candidate), Kind::Episode, Some(source)) => {
                Some(fetch_episodes(source, &candidate.id, &group.files))
            }
            _ => None,
        };
        for file in &group.files {
            let mut item = base_item(&group, file, candidate.as_ref());
            match group.kind {
                Kind::Movie => {
                    item.edition = file.parsed.edition.clone();
                    item.part = file.parsed.part;
                }
                Kind::Episode => {
                    if !apply_episode(&mut item, &file.parsed, episodes.as_deref()) {
                        importer.summary.skipped += 1;
                        continue;
                    }
                }
            }
            match importer.import(item, file) {
                Ok(true) => importer.summary.imported += 1,
                Ok(false) => importer.summary.skipped += 1,
                Err(err) => {
                    eprintln!("  error: {err:#}");
                    importer.summary.skipped += 1;
                }
            }
        }
    }

    let Summary { imported, skipped } = importer.summary;
    let verb = if options.pretend {
        "Would import"
    } else {
        "Imported"
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

    fn episodes() -> Vec<EpisodeInfo> {
        let episode = |season, number, title: &str, date: &str| EpisodeInfo {
            season,
            number,
            title: Some(title.to_string()),
            air_date: Some(date.to_string()),
        };
        vec![
            episode(1, 1, "Pilot (1)", "2010-01-01"),
            episode(1, 2, "Pilot (2)", "2010-01-01"),
            episode(8, 20, "Elijah Wood", "2011-11-15"),
        ]
    }

    fn applied(name: &str, episodes: Option<&[EpisodeInfo]>) -> Option<Item> {
        let parsed = parse::parse_name(name);
        let mut item = Item::new(Kind::Episode, "/in/file.mkv".into(), parsed.title.clone());
        apply_episode(&mut item, &parsed, episodes).then_some(item)
    }

    #[test]
    fn episode_titles_come_from_metadata() {
        let item = applied("Show S01E01-E02 Wrong Title", Some(&episodes())).unwrap();
        assert_eq!(item.episode_title.as_deref(), Some("Pilot (1) & Pilot (2)"));
        assert_eq!(item.episode_code().as_deref(), Some("s01e01-e02"));
        assert_eq!(item.air_date.as_deref(), Some("2010-01-01"));
    }

    #[test]
    fn dates_resolve_to_seasons() {
        let item = applied("Show 2011.11.15 HDTV", Some(&episodes())).unwrap();
        assert_eq!((item.season, item.episode), (Some(8), Some(20)));
        assert_eq!(item.episode_title.as_deref(), Some("Elijah Wood"));
        assert_eq!(item.episode_code().as_deref(), Some("2011-11-15"));
        assert!(applied("Show 2011.11.16 HDTV", Some(&episodes())).is_none());
    }

    #[test]
    fn as_is_keeps_parsed_titles() {
        let item = applied("Show S01E03 Parsed Title 720p", None).unwrap();
        assert_eq!(item.episode_title.as_deref(), Some("Parsed Title"));
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
}
