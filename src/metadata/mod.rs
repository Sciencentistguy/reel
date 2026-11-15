//! Online metadata sources and candidate scoring.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;

use crate::config::{MetadataConfig, TvSource};

pub mod tmdb;
pub mod tvdb;

/// A film or show as known to a metadata source.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub source: &'static str,
    pub id: String,
    pub title: String,
    pub year: Option<i32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EpisodeInfo {
    pub season: u32,
    pub number: u32,
    pub title: Option<String>,
    pub air_date: Option<String>,
}

pub trait MovieSource {
    fn search_movies(&self, title: &str, year: Option<i32>) -> Result<Vec<Candidate>>;
    fn movie(&self, id: &str) -> Result<Candidate>;
}

pub trait ShowSource {
    fn name(&self) -> &'static str;
    fn search_shows(&self, title: &str, year: Option<i32>) -> Result<Vec<Candidate>>;
    fn show(&self, id: &str) -> Result<Candidate>;
    /// Episodes of one season, or of the whole show when `season` is `None`.
    fn episodes(&self, id: &str, season: Option<u32>) -> Result<Vec<EpisodeInfo>>;
}

pub fn movie_source(config: &MetadataConfig) -> Result<Box<dyn MovieSource>> {
    Ok(Box::new(tmdb::Tmdb::new(config)?))
}

pub fn show_source(config: &MetadataConfig) -> Result<Box<dyn ShowSource>> {
    Ok(match config.tv_source {
        TvSource::Tvdb => Box::new(tvdb::Tvdb::new(config)?),
        TvSource::Tmdb => Box::new(tmdb::Tmdb::new(config)?),
    })
}

pub(crate) fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .user_agent(concat!("vlib/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

pub(crate) fn read_json<T: DeserializeOwned>(
    service: &str,
    result: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
) -> Result<T> {
    match result {
        Ok(mut response) => response
            .body_mut()
            .read_json()
            .with_context(|| format!("unexpected response from {service}")),
        Err(ureq::Error::StatusCode(401)) => {
            bail!("{service} rejected the API key (HTTP 401)")
        }
        Err(ureq::Error::StatusCode(404)) => bail!("{service}: not found (HTTP 404)"),
        Err(err) => Err(err).with_context(|| format!("{service} request failed")),
    }
}

pub(crate) fn year_of(date: Option<&str>) -> Option<i32> {
    date?.get(..4)?.parse().ok()
}

fn normalise(title: &str) -> String {
    let lowered = title.to_lowercase().replace('&', " and ");
    let kept: String = lowered
        .chars()
        .filter_map(|c| match c {
            c if c.is_alphanumeric() => Some(c),
            '\'' | '’' => None,
            _ => Some(' '),
        })
        .collect();
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Distance between what was parsed from a file and a candidate: 0 is a perfect match, 1 is
/// nothing alike.
pub fn distance(title: &str, year: Option<i32>, candidate: &Candidate) -> f64 {
    let title_distance =
        1.0 - strsim::normalized_levenshtein(&normalise(title), &normalise(&candidate.title));
    let year_penalty = match (year, candidate.year) {
        (Some(wanted), Some(actual)) => match (wanted - actual).abs() {
            0 => 0.0,
            1 => 0.05,
            _ => 0.25,
        },
        (Some(_), None) => 0.1,
        (None, _) => 0.02,
    };
    (title_distance + year_penalty).min(1.0)
}

/// Sorts candidates by distance, preserving the source's own ranking among ties.
pub fn rank(title: &str, year: Option<i32>, candidates: Vec<Candidate>) -> Vec<(f64, Candidate)> {
    let mut ranked: Vec<_> = candidates
        .into_iter()
        .map(|candidate| (distance(title, year, &candidate), candidate))
        .collect();
    ranked.sort_by(|a, b| a.0.total_cmp(&b.0));
    ranked
}

/// Whether the best candidate is good enough, and clearly enough ahead, to apply unprompted.
pub fn is_strong(ranked: &[(f64, Candidate)], threshold: f64) -> bool {
    match ranked {
        [] => false,
        [(best, _)] => *best <= threshold,
        [(best, _), (next, _), ..] => *best <= threshold && next - best >= 0.04,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(title: &str, year: i32) -> Candidate {
        Candidate {
            source: "tmdb",
            id: "1".into(),
            title: title.into(),
            year: Some(year),
        }
    }

    #[test]
    fn exact_match_is_strong() {
        let ranked = rank(
            "the matrix",
            Some(1999),
            vec![
                candidate("The Matrix Reloaded", 2003),
                candidate("The Matrix", 1999),
            ],
        );
        assert_eq!(ranked[0].1.title, "The Matrix");
        assert_eq!(ranked[0].0, 0.0);
        assert!(is_strong(&ranked, 0.08));
    }

    #[test]
    fn remakes_without_a_year_are_ambiguous() {
        let ranked = rank(
            "Dune",
            None,
            vec![candidate("Dune", 2021), candidate("Dune", 1984)],
        );
        assert!(!is_strong(&ranked, 0.08));
        let ranked = rank(
            "Dune",
            Some(1984),
            vec![candidate("Dune", 2021), candidate("Dune", 1984)],
        );
        assert_eq!(ranked[0].1.year, Some(1984));
        assert!(is_strong(&ranked, 0.08));
    }

    #[test]
    fn punctuation_is_ignored() {
        let c = candidate("Marvel's Agents of S.H.I.E.L.D.", 2013);
        assert!(distance("Marvels Agents of S H I E L D", Some(2013), &c) < 0.01);
        assert!(distance("Completely Different", Some(2013), &c) > 0.5);
    }
}
