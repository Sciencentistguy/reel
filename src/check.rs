//! `reel check`: episode files whose running time doesn't fit the episode they are matched to.

use std::collections::HashMap;

use anyhow::Result;

use crate::config::Config;
use crate::library::Library;
use crate::metadata::{self, EpisodeInfo, ShowSource};
use crate::missing::shows;
use crate::model::Item;
use crate::query::Query;
use crate::sync::{check_length, fetch_episodes, listed_episodes};

#[derive(clap::Args)]
pub struct Options {
    pub query: Vec<String>,
}

/// What checking one show's files found.
#[derive(Debug, Default, PartialEq)]
struct Review {
    /// Each misfit's index among the show's files, and why it doesn't fit.
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
        0 => println!("Every checked episode file fits the episode it is matched to."),
        total => println!(
            "{total} file(s) run too long or short to be the episode they are matched to; check \
             their numbering, then `reel remove` and `reel rescan` them."
        ),
    }
    if unlisted > 0 {
        println!("{unlisted} file(s) are numbered as episodes their source doesn't list.");
    }
    if unprobed > 0 {
        println!(
            "{unprobed} file(s) have no known running time; `reel update --force` probes them."
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
        if item.media.duration.is_none() {
            review.unprobed += 1;
            continue;
        }
        let (found, complete) = listed_episodes(item, episodes);
        if !complete {
            review.unlisted += 1;
            continue;
        }
        if let Err(misfit) = check_length(item, &found) {
            review.misfits.push((index, misfit));
        }
    }
    review
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
