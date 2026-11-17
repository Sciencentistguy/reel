//! `reel sync`: refresh identified items from the metadata source they were matched against.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use anyhow::Result;

use crate::commands::relocate;
use crate::config::Config;
use crate::library::Library;
use crate::metadata::{self, Candidate, EpisodeInfo, MovieSource, ShowSource};
use crate::model::{Item, Kind};
use crate::query::Query;

#[derive(clap::Args)]
pub struct Options {
    /// Show what would change without touching files or the database.
    #[arg(short = 'n', long, visible_alias = "dry-run")]
    pub pretend: bool,
    /// Update the database but leave files where they are.
    #[arg(short = 'M', long)]
    pub no_move: bool,
    pub query: Vec<String>,
}

/// Everything fetched about one film or show.
struct Record {
    work: Candidate,
    episodes: Vec<EpisodeInfo>,
}

pub fn run(config: &Config, library: &mut Library, options: &Options) -> Result<()> {
    let mut movies: Option<Box<dyn MovieSource>> = None;
    let mut shows: HashMap<String, Box<dyn ShowSource>> = HashMap::new();
    let mut claimed = HashSet::new();
    let (mut changed, mut moved, mut failed) = (0, 0, 0);

    for ((kind, source, id), mut items) in works(library.query(&Query::parse(&options.query)?)?) {
        let record = match kind {
            Kind::Movie => {
                if movies.is_none() {
                    movies = Some(metadata::movie_source(&config.metadata)?);
                }
                movies.as_deref().unwrap().movie(&id).map(|work| Record {
                    work,
                    episodes: Vec::new(),
                })
            }
            Kind::Episode => {
                if !shows.contains_key(&source) {
                    let client = metadata::show_source_named(&config.metadata, &source)?;
                    shows.insert(source.clone(), client);
                }
                fetch_show(shows[&source].as_ref(), &id, &items)
            }
        };
        let record = match record {
            Ok(record) => record,
            Err(err) => {
                eprintln!("warning: cannot fetch {source}-{id}: {err:#}");
                failed += items.len();
                continue;
            }
        };

        for item in &mut items {
            let before = item.clone();
            apply(item, &record);
            let differences = differences(&before, item);
            if !differences.is_empty() {
                println!("{}", before.path.display());
                for difference in &differences {
                    println!("  {difference}");
                }
                changed += 1;
            }
            let relocated =
                !options.no_move && relocate(config, library, item, &mut claimed, options.pretend)?;
            moved += usize::from(relocated);
            if !options.pretend && (relocated || !differences.is_empty()) {
                library.save(item)?;
            }
        }
    }

    let (updated, renamed) = if options.pretend {
        ("Would update", "would move")
    } else {
        ("Updated", "moved")
    };
    print!("{updated} {changed} item(s); {renamed} {moved} file(s).");
    if failed > 0 {
        print!(" Could not fetch metadata for {failed} item(s).");
    }
    println!();
    Ok(())
}

/// Groups identified items by the work they were matched to, so each is fetched once.
fn works(items: Vec<Item>) -> BTreeMap<(Kind, String, String), Vec<Item>> {
    let mut works: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for item in items {
        if let (Some(source), Some(id)) = (item.source.clone(), item.source_id.clone()) {
            works.entry((item.kind, source, id)).or_default().push(item);
        }
    }
    works
}

fn fetch_show(source: &dyn ShowSource, id: &str, items: &[Item]) -> Result<Record> {
    let work = source.show(id)?;
    let episodes = if items.iter().any(|item| item.date_based) {
        source.episodes(id, None)?
    } else {
        let seasons: BTreeSet<u32> = items.iter().filter_map(|item| item.season).collect();
        let mut episodes = Vec::new();
        for season in seasons {
            episodes.extend(source.episodes(id, Some(season))?);
        }
        episodes
    };
    Ok(Record { work, episodes })
}

/// Brings an item's metadata in line with what its source now says.
fn apply(item: &mut Item, record: &Record) {
    item.title = record.work.title.clone();
    item.year = record.work.year.or(item.year);
    if item.kind == Kind::Episode {
        apply_episode(item, &record.episodes);
    }
}

fn apply_episode(item: &mut Item, episodes: &[EpisodeInfo]) {
    if item.date_based {
        let Some(found) = episodes
            .iter()
            .find(|episode| episode.air_date.is_some() && episode.air_date == item.air_date)
        else {
            return;
        };
        item.season = Some(found.season);
        item.episode = Some(found.number);
        item.episode_title = found.title.clone().or(item.episode_title.take());
        return;
    }

    let (Some(season), Some(first)) = (item.season, item.episode) else {
        return;
    };
    let last = item.episode_end.unwrap_or(first);
    let found: Vec<&EpisodeInfo> = (first..=last)
        .filter_map(|number| {
            episodes
                .iter()
                .find(|episode| episode.season == season && episode.number == number)
        })
        .collect();
    let Some(opening) = found.first() else {
        return;
    };
    let mut titles: Vec<&str> = Vec::new();
    for title in found.iter().filter_map(|episode| episode.title.as_deref()) {
        if !titles.contains(&title) {
            titles.push(title);
        }
    }
    if !titles.is_empty() {
        item.episode_title = Some(titles.join(" & "));
    }
    if opening.air_date.is_some() {
        item.air_date = opening.air_date.clone();
    }
}

/// The metadata fields that differ between two versions of an item, as `field: old -> new`.
fn differences(before: &Item, after: &Item) -> Vec<String> {
    let show = |value: Option<String>| value.unwrap_or_else(|| "(none)".into());
    let fields = [
        (
            "title",
            Some(before.title.clone()),
            Some(after.title.clone()),
        ),
        (
            "year",
            before.year.map(|y| y.to_string()),
            after.year.map(|y| y.to_string()),
        ),
        (
            "season",
            before.season.map(|s| s.to_string()),
            after.season.map(|s| s.to_string()),
        ),
        (
            "episode",
            before.episode.map(|e| e.to_string()),
            after.episode.map(|e| e.to_string()),
        ),
        (
            "episode_title",
            before.episode_title.clone(),
            after.episode_title.clone(),
        ),
        ("air_date", before.air_date.clone(), after.air_date.clone()),
    ];
    fields
        .into_iter()
        .filter(|(_, old, new)| old != new)
        .map(|(field, old, new)| format!("{field}: {} -> {}", show(old), show(new)))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn work(title: &str, year: Option<i32>) -> Candidate {
        Candidate {
            source: "tvdb",
            id: "1".into(),
            title: title.into(),
            year,
            url: String::new(),
            original_title: None,
            overview: None,
            runtime: None,
        }
    }

    fn episode(season: u32, number: u32, title: Option<&str>, air_date: &str) -> EpisodeInfo {
        EpisodeInfo {
            season,
            number,
            title: title.map(Into::into),
            air_date: Some(air_date.into()),
        }
    }

    fn item(kind: Kind, source_id: Option<&str>) -> Item {
        let mut item = Item::new(kind, PathBuf::from("/library/x.mkv"), "Old".into());
        item.source = source_id.map(|_| "tvdb".into());
        item.source_id = source_id.map(Into::into);
        item
    }

    #[test]
    fn works_group_identified_items_and_ignore_the_rest() {
        let grouped = works(vec![
            item(Kind::Episode, Some("1")),
            item(Kind::Episode, Some("1")),
            item(Kind::Episode, Some("2")),
            item(Kind::Movie, Some("1")),
            item(Kind::Movie, None),
        ]);
        let sizes: Vec<_> = grouped
            .iter()
            .map(|((kind, _, id), items)| (*kind, id.as_str(), items.len()))
            .collect();
        assert_eq!(
            sizes,
            [
                (Kind::Movie, "1", 1),
                (Kind::Episode, "1", 2),
                (Kind::Episode, "2", 1)
            ]
        );
    }

    #[test]
    fn films_take_the_current_title_and_keep_a_known_year() {
        let mut film = item(Kind::Movie, Some("1"));
        film.year = Some(1995);
        let record = Record {
            work: work("New", None),
            episodes: Vec::new(),
        };
        apply(&mut film, &record);
        assert_eq!((film.title.as_str(), film.year), ("New", Some(1995)));
    }

    #[test]
    fn episodes_pick_up_new_titles_and_air_dates() {
        let record = Record {
            work: work("Show", Some(2020)),
            episodes: vec![
                episode(1, 1, Some("Pilot"), "2020-01-01"),
                episode(1, 2, Some("Part One"), "2020-01-08"),
                episode(1, 3, Some("Part Two"), "2020-01-08"),
                episode(1, 4, None, "2020-01-15"),
            ],
        };

        let mut single = item(Kind::Episode, Some("1"));
        (single.season, single.episode) = (Some(1), Some(1));
        single.episode_title = Some("TBA".into());
        apply(&mut single, &record);
        assert_eq!(single.title, "Show");
        assert_eq!(single.episode_title.as_deref(), Some("Pilot"));
        assert_eq!(single.air_date.as_deref(), Some("2020-01-01"));

        let mut double = item(Kind::Episode, Some("1"));
        (double.season, double.episode, double.episode_end) = (Some(1), Some(2), Some(3));
        apply(&mut double, &record);
        assert_eq!(double.episode_title.as_deref(), Some("Part One & Part Two"));

        let mut untitled = item(Kind::Episode, Some("1"));
        (untitled.season, untitled.episode) = (Some(1), Some(4));
        untitled.episode_title = Some("Kept".into());
        apply(&mut untitled, &record);
        assert_eq!(untitled.episode_title.as_deref(), Some("Kept"));

        let mut unknown = item(Kind::Episode, Some("1"));
        (unknown.season, unknown.episode) = (Some(9), Some(1));
        unknown.air_date = Some("1999-01-01".into());
        apply(&mut unknown, &record);
        assert_eq!(unknown.air_date.as_deref(), Some("1999-01-01"));
    }

    #[test]
    fn date_based_episodes_are_found_by_air_date() {
        let record = Record {
            work: work("Daily", None),
            episodes: vec![episode(2024, 37, Some("Guest"), "2024-03-01")],
        };
        let mut daily = item(Kind::Episode, Some("1"));
        daily.date_based = true;
        daily.air_date = Some("2024-03-01".into());
        apply(&mut daily, &record);
        assert_eq!((daily.season, daily.episode), (Some(2024), Some(37)));
        assert_eq!(daily.episode_title.as_deref(), Some("Guest"));
    }

    #[test]
    fn differences_list_changed_fields() {
        let before = item(Kind::Episode, Some("1"));
        let mut after = before.clone();
        after.title = "New".into();
        after.episode_title = Some("Pilot".into());
        assert_eq!(
            differences(&before, &after),
            ["title: Old -> New", "episode_title: (none) -> Pilot"]
        );
        assert!(differences(&before, &before).is_empty());
    }
}
