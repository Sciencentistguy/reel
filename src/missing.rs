//! `reel missing`: list the episodes of identified shows that the library has no file for.

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
    let (shows, unidentified) = shows(items);
    let today = today();
    let mut sources: HashMap<String, Box<dyn ShowSource>> = HashMap::new();
    let (mut total, mut failed) = (0, 0);

    for ((source, id), items) in shows {
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
    if unidentified > 0 {
        println!(
            "Skipped {unidentified} episode file(s) not matched to a metadata source; \
             import them without --asis to include their shows."
        );
    }
    Ok(())
}

/// Groups episode files by the show they were matched to, counting those matched to none.
fn shows(items: Vec<Item>) -> (BTreeMap<(String, String), Vec<Item>>, usize) {
    let mut shows: BTreeMap<_, Vec<_>> = BTreeMap::new();
    let mut unidentified = 0;
    for item in items.into_iter().filter(|item| item.kind == Kind::Episode) {
        match (item.source.clone(), item.source_id.clone()) {
            (Some(source), Some(id)) => shows.entry((source, id)).or_default().push(item),
            _ => unidentified += 1,
        }
    }
    (shows, unidentified)
}

/// The aired episodes no file covers, in the order the source lists them.
fn missing<'a>(
    items: &[Item],
    episodes: &'a [EpisodeInfo],
    specials: bool,
    today: &str,
) -> Vec<&'a EpisodeInfo> {
    let mut held = HashSet::new();
    let mut dates = HashSet::new();
    for item in items {
        if item.date_based {
            dates.extend(item.air_date.as_deref());
        } else if let (Some(season), Some(first)) = (item.season, item.episode) {
            for number in first..=item.episode_end.unwrap_or(first).max(first) {
                held.insert((season, number));
            }
        }
    }
    episodes
        .iter()
        .filter(|episode| specials || episode.season != 0)
        .filter(|episode| episode.air_date.as_deref().is_none_or(|date| date <= today))
        .filter(|episode| !held.contains(&(episode.season, episode.number)))
        .filter(|episode| {
            episode
                .air_date
                .as_deref()
                .is_none_or(|date| !dates.contains(date))
        })
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
    fn shows_group_identified_episodes_and_count_the_rest() {
        let identified = |id: &str| {
            let mut item = file(1, 1, None);
            item.source = Some("tvdb".into());
            item.source_id = Some(id.into());
            item
        };
        let film = Item::new(Kind::Movie, PathBuf::from("/m.mkv"), "Film".into());
        let (shows, unidentified) = shows(vec![
            identified("1"),
            identified("1"),
            identified("2"),
            file(1, 1, None),
            film,
        ]);
        let sizes: Vec<_> = shows
            .iter()
            .map(|((_, id), items)| (id.as_str(), items.len()))
            .collect();
        assert_eq!(sizes, [("1", 2), ("2", 1)]);
        assert_eq!(unidentified, 1);
    }

    #[test]
    fn civil_dates_are_computed_from_days_since_the_epoch() {
        assert_eq!(civil_date(0), (1970, 1, 1));
        assert_eq!(civil_date(11_016), (2000, 2, 29));
        assert_eq!(civil_date(20_775), (2026, 11, 18));
    }
}
