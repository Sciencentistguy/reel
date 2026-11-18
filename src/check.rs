//! `reel check`: episode files that don't look like the episode they are matched to, either
//! because their running time doesn't fit it or because their own title names another episode.

use std::collections::HashMap;
use std::iter;

use anyhow::Result;

use crate::config::Config;
use crate::import::describe_episodes;
use crate::library::Library;
use crate::metadata::{self, EpisodeInfo, ShowSource, normalise};
use crate::missing::shows;
use crate::model::Item;
use crate::parse;
use crate::query::Query;
use crate::sync::{check_length, fetch_episodes, listed_episodes};

/// How alike a file's title and an episode's must be (normalised Levenshtein similarity) for the
/// file to be taken as naming that episode.
const SAME_TITLE: f64 = 0.8;
/// How much more alike the other episode's title must be than the matched episode's, so that
/// near-identical titles such as `Part 1` and `Part 2` are never told apart on this alone.
const CLEAR_MARGIN: f64 = 0.2;

#[derive(clap::Args)]
pub struct Options {
    pub query: Vec<String>,
}

/// What checking one show's files found.
#[derive(Debug, Default, PartialEq)]
struct Review {
    /// Each doubtful file's index among the show's files, and why it is doubtful.
    misfits: Vec<(usize, String)>,
    /// Files numbered as episodes the source doesn't list (or lists only some of).
    unlisted: usize,
    /// Files whose running time was never probed.
    unprobed: usize,
}

pub fn run(config: &Config, library: &Library, options: &Options) -> Result<()> {
    let items = library.query(&Query::parse(&options.query)?)?;
    let mut sources: HashMap<String, Box<dyn ShowSource>> = HashMap::new();
    let (mut misfits, mut unlisted, mut unprobed, mut failed) = (0, 0, 0, 0);

    for ((source, id), mut items) in shows(items) {
        if !sources.contains_key(&source) {
            let client = metadata::show_source_named(&config.metadata, &source)?;
            sources.insert(source.clone(), client);
        }
        items.sort_by(|a, b| a.path.cmp(&b.path));
        let episodes = match fetch_episodes(sources[&source].as_ref(), &id, &items) {
            Ok(episodes) => episodes,
            Err(err) => {
                eprintln!("warning: cannot fetch {source}-{id}: {err:#}");
                failed += 1;
                continue;
            }
        };
        let review = review(&items, &episodes);
        unlisted += review.unlisted;
        unprobed += review.unprobed;
        if review.misfits.is_empty() {
            continue;
        }
        let show = &items[0];
        match show.year {
            Some(year) => println!("{} ({year}) [{source}-{id}]", show.title),
            None => println!("{} [{source}-{id}]", show.title),
        }
        for (index, misfit) in &review.misfits {
            let path = &items[*index].path;
            let shown = path.strip_prefix(&config.library.tv).unwrap_or(path);
            println!("  {}: {misfit}", shown.display());
        }
        misfits += review.misfits.len();
    }

    match misfits {
        0 => println!("Every checked episode file looks like the episode it is matched to."),
        total => println!(
            "{total} file(s) don't look like the episode they are matched to; check their \
             numbering, then `reel remove` and `reel rescan` them."
        ),
    }
    if unlisted > 0 {
        println!("{unlisted} file(s) are numbered as episodes their source doesn't list.");
    }
    if unprobed > 0 {
        println!(
            "{unprobed} file(s) have no known running time or title tag; `reel update --force` \
             probes them."
        );
    }
    if failed > 0 {
        println!("Could not fetch episode lists for {failed} show(s).");
    }
    Ok(())
}

/// Checks each of a show's files against the source's `episodes`.
fn review(items: &[Item], episodes: &[EpisodeInfo]) -> Review {
    let mut review = Review::default();
    for (index, item) in items.iter().enumerate() {
        let (found, complete) = listed_episodes(item, episodes);
        let mut problems = Vec::new();
        if let Some((claim, other)) = titled_as(item, &found, episodes) {
            let matched = if found.is_empty() {
                String::new()
            } else {
                format!("matched to {} but ", describe_episodes(&found))
            };
            problems.push(format!(
                "{matched}titled {claim:?}, which is {}",
                describe_episodes(&[other])
            ));
        }
        if complete {
            match item.media.duration {
                Some(_) => problems.extend(check_length(item, &found).err()),
                None => review.unprobed += 1,
            }
        } else if problems.is_empty() {
            review.unlisted += 1;
        }
        if !problems.is_empty() {
            review.misfits.push((index, problems.join("; ")));
        }
    }
    review
}

/// The titles a file gives for the episode it holds: its container's title tag and the episode
/// titles in its current and original names. A tag that numbers the episode (`Show S05E05
/// Title`) gives only the title after the number.
fn claimed_titles(item: &Item) -> Vec<String> {
    let mut claims = Vec::new();
    if let Some(tag) = &item.media.title {
        let parsed = parse::parse_name(tag);
        if parsed.episode.is_some() || parsed.air_date.is_some() {
            claims.extend(parsed.episode_title);
        } else {
            claims.push(tag.clone());
        }
    }
    for path in iter::once(&item.path).chain(&item.original_path) {
        claims.extend(parse::parse(path).episode_title);
    }
    claims
}

/// Another of the show's `episodes` that one of the file's own titles clearly names, when none of
/// them names an episode it is matched to (`own`).
fn titled_as<'a>(
    item: &Item,
    own: &[&EpisodeInfo],
    episodes: &'a [EpisodeInfo],
) -> Option<(String, &'a EpisodeInfo)> {
    let claims: Vec<_> = claimed_titles(item)
        .into_iter()
        .map(|claim| (normalise(&claim), claim))
        .filter(|(normalised, _)| !normalised.is_empty())
        .collect();
    let likeness = |claim: &str, episode: &EpisodeInfo| {
        episode.title.as_deref().map_or(0.0, |title| {
            strsim::normalized_levenshtein(claim, &normalise(title))
        })
    };
    let own_likeness = |claim: &str| {
        own.iter()
            .map(|episode| likeness(claim, episode))
            .fold(0.0, f64::max)
    };
    if claims
        .iter()
        .any(|(claim, _)| own_likeness(claim) >= SAME_TITLE)
    {
        return None;
    }
    claims.into_iter().find_map(|(claim, shown)| {
        let mine = own_likeness(&claim);
        let (best, score) = episodes
            .iter()
            .filter(|episode| !own.iter().any(|own| std::ptr::eq(*own, *episode)))
            .map(|episode| (episode, likeness(&claim, episode)))
            .max_by(|a, b| a.1.total_cmp(&b.1))?;
        (score >= SAME_TITLE && score - mine >= CLEAR_MARGIN).then_some((shown, best))
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::model::Kind;

    fn episode(number: u32, title: &str, runtime: u32) -> EpisodeInfo {
        EpisodeInfo {
            season: 5,
            number,
            title: Some(title.into()),
            air_date: None,
            runtime: Some(runtime),
        }
    }

    fn file(first: u32, last: Option<u32>, seconds: Option<f64>) -> Item {
        let mut item = Item::new(Kind::Episode, PathBuf::from("/tv/x.mkv"), "Show".into());
        (item.season, item.episode, item.episode_end) = (Some(5), Some(first), last);
        item.media.duration = seconds;
        item
    }

    fn tagged(number: u32, tag: &str) -> Item {
        let mut item = file(number, None, Some(1300.0));
        item.media.title = Some(tag.into());
        item
    }

    fn office() -> [EpisodeInfo; 4] {
        [
            episode(5, "Employee Transfer", 22),
            episode(6, "Customer Survey", 22),
            episode(14, "Lecture Circuit (1)", 22),
            episode(15, "Lecture Circuit (2)", 22),
        ]
    }

    #[test]
    fn files_too_long_or_short_for_their_episode_are_misfits() {
        let episodes = [
            episode(27, "Escapism", 10),
            episode(28, "Change Your Mind", 45),
        ];
        let items = [
            file(27, None, Some(670.0)),
            file(28, None, Some(670.0)),
            file(27, Some(28), Some(3300.0)),
        ];
        assert_eq!(
            review(&items, &episodes),
            Review {
                misfits: vec![(
                    1,
                    "runs 0h 11m but s05e28 Change Your Mind runs 0h 45m".into()
                )],
                ..Review::default()
            }
        );
    }

    #[test]
    fn swapped_episodes_are_caught_by_their_title_tags() {
        let items = [
            tagged(5, "The Office S05E06 Customer Survey"),
            tagged(6, "The Office S05E05 Employee Transfer"),
        ];
        assert_eq!(
            review(&items, &office()).misfits,
            [
                (
                    0,
                    "matched to s05e05 Employee Transfer but titled \"Customer Survey\", which \
                     is s05e06 Customer Survey"
                        .into()
                ),
                (
                    1,
                    "matched to s05e06 Customer Survey but titled \"Employee Transfer\", which \
                     is s05e05 Employee Transfer"
                        .into()
                ),
            ]
        );
    }

    #[test]
    fn titles_in_file_names_count_too() {
        let mut item = file(5, None, Some(1300.0));
        item.original_path = Some("/in/The.Office.S05E05.Customer.Survey.720p.mkv".into());
        let found = review(&[item], &office()).misfits;
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].1.contains("s05e06 Customer Survey"), "{found:?}");
    }

    #[test]
    fn titles_that_agree_or_are_too_close_to_call_are_not_flagged() {
        let items = [
            tagged(5, "The Office S05E05 Employee Transfer"),
            tagged(6, "Customer Survey"),
            tagged(14, "The Office S05E14 Lecture Circuit Pt 1"),
            tagged(15, "The Office Season 5 Disc 3"),
        ];
        assert_eq!(review(&items, &office()), Review::default());
    }

    #[test]
    fn a_title_can_place_a_file_numbered_as_an_unlisted_episode() {
        let item = tagged(9, "Customer Survey");
        let found = review(&[item], &office());
        assert_eq!(found.unlisted, 0);
        assert!(found.misfits[0].1.contains("s05e06"), "{found:?}");
    }

    #[test]
    fn unlisted_and_unprobed_files_are_counted_not_checked() {
        let episodes = [episode(1, "One", 22)];
        let items = [
            file(1, None, None),
            file(2, None, Some(60.0)),
            file(1, Some(2), Some(60.0)),
        ];
        assert_eq!(
            review(&items, &episodes),
            Review {
                misfits: Vec::new(),
                unlisted: 2,
                unprobed: 1,
            }
        );
    }
}
