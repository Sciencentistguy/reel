//! `reel missing`: list the episodes of identified shows that the library has no file for, and
//! the files in the TV folder that the database doesn't know about.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;

use crate::config::Config;
use crate::import;
use crate::library::Library;
use crate::metadata::{self, EpisodeInfo, ShowSource};
use crate::model::{Item, Kind};
use crate::parse;
use crate::query::Query;

#[derive(clap::Args)]
pub struct Options {
    /// Also list missing specials (season 0).
    #[arg(short, long)]
    pub specials: bool,
    pub query: Vec<String>,
}

pub fn run(config: &Config, library: &Library, options: &Options) -> Result<()> {
    let items = library.query(&Query::parse(&options.query)?)?;
    let tv = TvFolder::new(&config.library.tv);
    let mut strays = tv.untracked(library)?;
    let shows = shows(items);
    let today = today();
    let mut sources: HashMap<String, Box<dyn ShowSource>> = HashMap::new();
    let (mut total, mut on_disk, mut failed) = (0, 0, 0);

    for ((source, id), items) in shows {
        let folders: HashSet<OsString> = items
            .iter()
            .filter_map(|item| tv.show_folder(&item.path))
            .collect();
        let mut untracked: Vec<PathBuf> = folders
            .iter()
            .filter_map(|folder| strays.remove(folder))
            .flatten()
            .collect();
        untracked.sort();

        if !sources.contains_key(&source) {
            let client = metadata::show_source_named(&config.metadata, &source)?;
            sources.insert(source.clone(), client);
        }
        let episodes = sources[&source].episodes(&id, None).unwrap_or_else(|err| {
            eprintln!("warning: cannot fetch {source}-{id}: {err:#}");
            failed += 1;
            Vec::new()
        });
        let missing = missing(&items, &episodes, options.specials, &today);
        if missing.is_empty() && untracked.is_empty() {
            continue;
        }

        let show = &items[0];
        let mut header = match show.year {
            Some(year) => format!("{} ({year}) [{source}-{id}]", show.title),
            None => format!("{} [{source}-{id}]", show.title),
        };
        if !untracked.is_empty() {
            header += &format!(" - {} file(s) on disk not in the library", untracked.len());
        }
        println!("{header}");
        let present = Coverage::of_files(&untracked);
        for episode in &missing {
            let note = if present.covers(episode) {
                " (on disk, not in the library)"
            } else {
                ""
            };
            println!("  {}{note}", describe(episode));
        }
        if !untracked.is_empty() {
            println!("  Not in the library:");
            for path in &untracked {
                println!("    {}", tv.relative(path).display());
            }
        }
        total += missing.len();
        on_disk += untracked.len();
    }

    if options.query.is_empty() && !strays.is_empty() {
        println!(
            "Folders in {} with no show in the library:",
            config.library.tv.display()
        );
        for (folder, files) in &strays {
            println!("  {} ({} file(s))", folder.to_string_lossy(), files.len());
            on_disk += files.len();
        }
    }

    println!("{total} episode(s) missing.");
    if on_disk > 0 {
        println!("{on_disk} file(s) on disk are not in the library; `reel rescan` adds them.");
    }
    if failed > 0 {
        println!("Could not fetch episode lists for {failed} show(s).");
    }
    Ok(())
}

/// The library's TV folder, where each show has a folder of its own.
struct TvFolder {
    root: PathBuf,
    /// `root` with symlinks resolved, as found files' paths are.
    canonical: Option<PathBuf>,
}

impl TvFolder {
    fn new(root: &Path) -> Self {
        TvFolder {
            root: root.to_path_buf(),
            canonical: std::fs::canonicalize(root).ok(),
        }
    }

    fn relative<'a>(&self, path: &'a Path) -> &'a Path {
        self.canonical
            .as_deref()
            .and_then(|canonical| path.strip_prefix(canonical).ok())
            .or_else(|| path.strip_prefix(&self.root).ok())
            .unwrap_or(path)
    }

    /// The name of the show folder holding `path`, when it is inside one.
    fn show_folder(&self, path: &Path) -> Option<OsString> {
        let relative = self.relative(path);
        if relative == path || relative.components().count() < 2 {
            return None;
        }
        relative
            .components()
            .next()
            .map(|first| first.as_os_str().to_owned())
    }

    /// Video files in the TV folder that the database has no row for, by show folder.
    fn untracked(&self, library: &Library) -> Result<BTreeMap<OsString, Vec<PathBuf>>> {
        let mut strays: BTreeMap<_, Vec<_>> = BTreeMap::new();
        if self.canonical.is_none() {
            return Ok(strays);
        }
        let (files, _) = import::videos(std::slice::from_ref(&self.root))?;
        for (path, _) in files {
            if library.contains_path(&path)?
                || library.contains_path(&self.root.join(self.relative(&path)))?
            {
                continue;
            }
            if let Some(folder) = self.show_folder(&path) {
                strays.entry(folder).or_default().push(path);
            }
        }
        Ok(strays)
    }
}

/// Groups episode files by the show they were matched to.
fn shows(items: Vec<Item>) -> BTreeMap<(String, String), Vec<Item>> {
    let mut shows: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for item in items.into_iter().filter(|item| item.kind == Kind::Episode) {
        if let (Some(source), Some(id)) = (item.source.clone(), item.source_id.clone()) {
            shows.entry((source, id)).or_default().push(item);
        }
    }
    shows
}

/// The episodes some set of files holds, by number and by air date.
#[derive(Default)]
struct Coverage {
    numbers: HashSet<(u32, u32)>,
    dates: HashSet<String>,
}

impl Coverage {
    fn add(
        &mut self,
        season: Option<u32>,
        first: Option<u32>,
        last: Option<u32>,
        date: Option<&str>,
        date_based: bool,
    ) {
        if date_based {
            self.dates.extend(date.map(str::to_owned));
        } else if let (Some(season), Some(first)) = (season, first) {
            for number in first..=last.unwrap_or(first).max(first) {
                self.numbers.insert((season, number));
            }
        }
    }

    fn of_items(items: &[Item]) -> Self {
        let mut coverage = Coverage::default();
        for item in items {
            coverage.add(
                item.season,
                item.episode,
                item.episode_end,
                item.air_date.as_deref(),
                item.date_based,
            );
        }
        coverage
    }

    /// What files' names say they hold, for files not yet in the library.
    fn of_files(paths: &[PathBuf]) -> Self {
        let mut coverage = Coverage::default();
        for path in paths {
            let parsed = parse::parse(path);
            let date_based = parsed.air_date.is_some();
            coverage.add(
                parsed.season,
                parsed.episode,
                parsed.episode_end,
                parsed.air_date.as_deref(),
                date_based,
            );
        }
        coverage
    }

    fn covers(&self, episode: &EpisodeInfo) -> bool {
        self.numbers.contains(&(episode.season, episode.number))
            || episode
                .air_date
                .as_ref()
                .is_some_and(|date| self.dates.contains(date))
    }
}

/// The aired episodes no file covers, in the order the source lists them.
fn missing<'a>(
    items: &[Item],
    episodes: &'a [EpisodeInfo],
    specials: bool,
    today: &str,
) -> Vec<&'a EpisodeInfo> {
    let held = Coverage::of_items(items);
    episodes
        .iter()
        .filter(|episode| specials || episode.season != 0)
        .filter(|episode| episode.air_date.as_deref().is_none_or(|date| date <= today))
        .filter(|episode| !held.covers(episode))
        .collect()
}

fn describe(episode: &EpisodeInfo) -> String {
    let mut line = format!("s{:02}e{:02}", episode.season, episode.number);
    if let Some(title) = &episode.title {
        line += &format!(" {title}");
    }
    if let Some(date) = &episode.air_date {
        line += &format!(" ({date})");
    }
    line
}

/// Today's date in UTC as `YYYY-MM-DD`, the form metadata sources give air dates in.
fn today() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let (year, month, day) = civil_date(seconds / 86_400);
    format!("{year:04}-{month:02}-{day:02}")
}

/// The proleptic Gregorian date `days` after 1970-01-01 (Howard Hinnant's `civil_from_days`).
fn civil_date(days: u64) -> (u64, u64, u64) {
    let days = days + 719_468;
    let era = days / 146_097;
    let day_of_era = days % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn episode(season: u32, number: u32, air_date: Option<&str>) -> EpisodeInfo {
        EpisodeInfo {
            season,
            number,
            title: None,
            air_date: air_date.map(Into::into),
        }
    }

    fn file(season: u32, first: u32, last: Option<u32>) -> Item {
        let mut item = Item::new(Kind::Episode, PathBuf::from("/tv/x.mkv"), "Show".into());
        (item.season, item.episode, item.episode_end) = (Some(season), Some(first), last);
        item
    }

    fn codes(found: &[&EpisodeInfo]) -> Vec<(u32, u32)> {
        found
            .iter()
            .map(|episode| (episode.season, episode.number))
            .collect()
    }

    #[test]
    fn episodes_without_files_are_missing() {
        let episodes = [
            episode(1, 1, Some("2020-01-01")),
            episode(1, 2, Some("2020-01-08")),
            episode(1, 3, Some("2020-01-08")),
            episode(1, 4, Some("2020-01-15")),
            episode(2, 1, None),
        ];
        let items = [file(1, 1, None), file(1, 2, Some(3))];
        assert_eq!(
            codes(&missing(&items, &episodes, false, "2026-01-01")),
            [(1, 4), (2, 1)]
        );
    }

    #[test]
    fn specials_are_left_out_unless_asked_for() {
        let episodes = [episode(0, 1, None), episode(1, 1, None)];
        assert_eq!(
            codes(&missing(&[], &episodes, false, "2026-01-01")),
            [(1, 1)]
        );
        assert_eq!(
            codes(&missing(&[], &episodes, true, "2026-01-01")),
            [(0, 1), (1, 1)]
        );
    }

    #[test]
    fn episodes_yet_to_air_are_not_missing() {
        let episodes = [
            episode(1, 1, Some("2026-01-01")),
            episode(1, 2, Some("2026-01-02")),
        ];
        assert_eq!(
            codes(&missing(&[], &episodes, false, "2026-01-01")),
            [(1, 1)]
        );
    }

    #[test]
    fn date_based_files_cover_episodes_by_air_date() {
        let episodes = [
            episode(2024, 1, Some("2024-03-01")),
            episode(2024, 2, Some("2024-03-02")),
        ];
        let mut daily = file(0, 0, None);
        (daily.season, daily.episode) = (None, None);
        daily.date_based = true;
        daily.air_date = Some("2024-03-01".into());
        assert_eq!(
            codes(&missing(&[daily], &episodes, false, "2026-01-01")),
            [(2024, 2)]
        );
    }

    #[test]
    fn shows_group_episodes_by_their_match() {
        let identified = |id: &str| {
            let mut item = file(1, 1, None);
            item.source = Some("tvdb".into());
            item.source_id = Some(id.into());
            item
        };
        let mut film = Item::new(Kind::Movie, PathBuf::from("/m.mkv"), "Film".into());
        (film.source, film.source_id) = (Some("tmdb".into()), Some("1".into()));
        let shows = shows(vec![
            identified("1"),
            identified("1"),
            identified("2"),
            film,
        ]);
        let sizes: Vec<_> = shows
            .iter()
            .map(|((_, id), items)| (id.as_str(), items.len()))
            .collect();
        assert_eq!(sizes, [("1", 2), ("2", 1)]);
    }

    #[test]
    fn files_on_disk_but_not_in_the_database_are_found_by_show_folder() {
        let dir = tempfile::tempdir().unwrap();
        let write = |relative: &str| {
            let path = dir.path().join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "video").unwrap();
            path
        };
        let known = write("TV/Show {tvdb-1}/Season 01/Show - s01e01.mkv");
        write("TV/Show {tvdb-1}/Season 01/Show - s01e02.mkv");
        write("TV/Show {tvdb-1}/Season 01/Show - s01e02.en.srt");
        write("TV/Archer/Archer.S01E01.mkv");
        write("TV/loose.S01E01.mkv");

        let mut library = Library::in_memory().unwrap();
        let mut item = file(1, 1, None);
        item.path = known.clone();
        library.save(&mut item).unwrap();

        let tv = TvFolder::new(&dir.path().join("TV"));
        let strays = tv.untracked(&library).unwrap();
        let found: Vec<_> = strays
            .iter()
            .map(|(folder, files)| {
                let names: Vec<_> = files
                    .iter()
                    .map(|path| tv.relative(path).to_owned())
                    .collect();
                (folder.to_string_lossy().into_owned(), names)
            })
            .collect();
        assert_eq!(
            found,
            [
                (
                    "Archer".to_string(),
                    vec![PathBuf::from("Archer/Archer.S01E01.mkv")]
                ),
                (
                    "Show {tvdb-1}".to_string(),
                    vec![PathBuf::from("Show {tvdb-1}/Season 01/Show - s01e02.mkv")]
                ),
            ],
            "files outside a show folder and those in the database are left out"
        );
        assert_eq!(tv.show_folder(&known), Some("Show {tvdb-1}".into()));
    }

    #[test]
    fn a_missing_tv_folder_has_nothing_untracked() {
        let tv = TvFolder::new(Path::new("/definitely/not/here"));
        assert!(
            tv.untracked(&Library::in_memory().unwrap())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn untracked_files_cover_the_episodes_their_names_give() {
        let present = Coverage::of_files(&[
            PathBuf::from("/tv/Show/Show - s01e02-e03.mkv"),
            PathBuf::from("/tv/Daily/Daily 2024-03-01.mkv"),
        ]);
        assert!(present.covers(&episode(1, 3, None)));
        assert!(!present.covers(&episode(1, 4, None)));
        assert!(present.covers(&episode(2024, 9, Some("2024-03-01"))));
    }

    #[test]
    fn civil_dates_are_computed_from_days_since_the_epoch() {
        assert_eq!(civil_date(0), (1970, 1, 1));
        assert_eq!(civil_date(11_016), (2000, 2, 29));
        assert_eq!(civil_date(20_775), (2026, 11, 18));
    }
}
