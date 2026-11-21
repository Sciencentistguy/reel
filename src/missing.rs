//! `reel missing`: the aired episodes of the library's shows that it has no file for.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;

use crate::config::Config;
use crate::library::Library;
use crate::metadata::{self, EpisodeInfo, ShowSource};
use crate::model::{Item, Kind};
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
    let today = today();
    let mut sources: HashMap<String, Box<dyn ShowSource>> = HashMap::new();
    let (mut total, mut failed) = (0, 0);

    for ((source, id), items) in shows(items) {
        if !sources.contains_key(&source) {
            let client = metadata::show_source_named(&config.metadata, &source)?;
            sources.insert(source.clone(), client);
        }
        let episodes = match sources[&source].episodes(&id, None) {
            Ok(episodes) => episodes,
            Err(err) => {
                eprintln!("warning: cannot fetch {source}-{id}: {err:#}");
                failed += 1;
                continue;
            }
        };
        let missing = missing(&items, &episodes, options.specials, &today);
        if missing.is_empty() {
            continue;
        }
        let show = &items[0];
        match show.year {
            Some(year) => println!("{} ({year}) [{source}-{id}]", show.title),
            None => println!("{} [{source}-{id}]", show.title),
        }
        for episode in &missing {
            println!("  {}", describe(episode));
        }
        total += missing.len();
    }

    println!("{total} episode(s) missing.");
    if failed > 0 {
        println!("Could not fetch episode lists for {failed} show(s).");
    }
    Ok(())
}

/// Groups episode files by the show they were matched to.
pub fn shows(items: Vec<Item>) -> BTreeMap<(String, String), Vec<Item>> {
    let mut shows: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for item in items.into_iter().filter(|item| item.kind == Kind::Episode) {
        if let (Some(source), Some(id)) = (item.source.clone(), item.source_id.clone()) {
            shows.entry((source, id)).or_default().push(item);
        }
    }
    shows
}

/// The episodes a show's files hold, by number and, for daily shows, by air date.
#[derive(Default)]
struct Coverage {
    numbers: HashSet<(u32, u32)>,
    dates: HashSet<String>,
}

impl Coverage {
    fn of(items: &[Item]) -> Self {
        let mut coverage = Coverage::default();
        for item in items {
            if item.date_based {
                coverage.dates.extend(item.air_date.clone());
                coverage.numbers.extend(item.season.zip(item.episode));
            } else if let (Some(season), Some(first)) = (item.season, item.episode) {
                for number in first..=item.episode_end.unwrap_or(first).max(first) {
                    coverage.numbers.insert((season, number));
                }
            }
        }
        coverage
    }

    /// Whether a file holds `episode`. A file's air date only stands for an episode that aired
    /// alone that day: of a day's sports fixtures, only the numbered one is held.
    fn covers(&self, episode: &EpisodeInfo, shared_dates: &HashSet<&str>) -> bool {
        self.numbers.contains(&(episode.season, episode.number))
            || episode
                .air_date
                .as_deref()
                .is_some_and(|date| self.dates.contains(date) && !shared_dates.contains(date))
    }
}

/// The aired episodes no file covers, in the order the source lists them.
fn missing<'a>(
    items: &[Item],
    episodes: &'a [EpisodeInfo],
    specials: bool,
    today: &str,
) -> Vec<&'a EpisodeInfo> {
    let held = Coverage::of(items);
    let mut seen = HashSet::new();
    let shared_dates: HashSet<&str> = episodes
        .iter()
        .filter_map(|episode| episode.air_date.as_deref())
        .filter(|&date| !seen.insert(date))
        .collect();
    episodes
        .iter()
        .filter(|episode| specials || episode.season != 0)
        .filter(|episode| episode.air_date.as_deref().is_none_or(|date| date <= today))
        .filter(|episode| !held.covers(episode, &shared_dates))
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
            runtime: None,
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
    fn a_fixture_covers_only_itself_on_a_day_of_several() {
        let episodes = [
            episode(2024, 1, Some("2024-03-02")),
            episode(2024, 2, Some("2024-03-02")),
            episode(2024, 3, Some("2024-03-03")),
        ];
        let mut fixture = file(2024, 2, None);
        fixture.date_based = true;
        fixture.air_date = Some("2024-03-02".into());
        assert_eq!(
            codes(&missing(&[fixture], &episodes, false, "2026-01-01")),
            [(2024, 1), (2024, 3)]
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
    fn civil_dates_are_computed_from_days_since_the_epoch() {
        assert_eq!(civil_date(0), (1970, 1, 1));
        assert_eq!(civil_date(11_016), (2000, 2, 29));
        assert_eq!(civil_date(20_775), (2026, 11, 18));
    }
}
