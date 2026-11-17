//! TheTVDB (API v4).

use std::cell::RefCell;
use std::collections::HashMap;

use anyhow::{Result, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::{
    Candidate, EpisodeInfo, ShowSource, agent, distinct_from, non_empty, read_json, year_of,
};
use crate::config::MetadataConfig;

const BASE: &str = "https://api4.thetvdb.com/v4";

pub struct Tvdb {
    agent: ureq::Agent,
    key: String,
    language: String,
    token: RefCell<Option<String>>,
}

#[derive(Deserialize)]
struct Envelope<T> {
    data: T,
    links: Option<Links>,
}

#[derive(Deserialize)]
struct Links {
    next: Option<String>,
}

#[derive(Deserialize)]
struct Login {
    token: String,
}

#[derive(Deserialize)]
struct SearchResult {
    tvdb_id: String,
    name: String,
    year: Option<String>,
    first_air_time: Option<String>,
    overview: Option<String>,
    #[serde(default)]
    translations: Option<HashMap<String, String>>,
    #[serde(default)]
    overviews: Option<HashMap<String, String>>,
}

#[derive(Deserialize)]
struct Series {
    id: u64,
    name: String,
    year: Option<String>,
    #[serde(rename = "firstAired")]
    first_aired: Option<String>,
    overview: Option<String>,
}

#[derive(Deserialize)]
struct Translation {
    name: Option<String>,
    overview: Option<String>,
}

#[derive(Deserialize)]
struct EpisodePage {
    #[serde(default)]
    episodes: Vec<Episode>,
}

#[derive(Deserialize)]
struct Episode {
    #[serde(rename = "seasonNumber")]
    season: Option<u32>,
    number: Option<u32>,
    name: Option<String>,
    aired: Option<String>,
}

fn series_url(id: &str) -> String {
    format!("https://thetvdb.com/?tab=series&id={id}")
}

impl Tvdb {
    pub fn new(config: &MetadataConfig) -> Result<Self> {
        if config.tvdb_api_key.is_empty() {
            bail!(
                "no TVDB API key configured: set metadata.tvdb_api_key or TVDB_API_KEY \
                 (https://thetvdb.com/api-information), set metadata.tv_source = \"tmdb\", \
                 or import with --asis"
            );
        }
        Ok(Tvdb {
            agent: agent(),
            key: config.tvdb_api_key.clone(),
            language: config.tvdb_language.clone(),
            token: RefCell::new(None),
        })
    }

    fn token(&self) -> Result<String> {
        if let Some(token) = self.token.borrow().as_ref() {
            return Ok(token.clone());
        }
        let response = self
            .agent
            .post(format!("{BASE}/login"))
            .send_json(serde_json::json!({ "apikey": self.key }));
        let login: Envelope<Login> = read_json("TVDB", response)?;
        *self.token.borrow_mut() = Some(login.data.token.clone());
        Ok(login.data.token)
    }

    fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<Envelope<T>> {
        let mut request = self
            .agent
            .get(format!("{BASE}{path}"))
            .header("Authorization", format!("Bearer {}", self.token()?));
        for (name, value) in query {
            request = request.query(*name, value);
        }
        read_json("TVDB", request.call())
    }
}

impl ShowSource for Tvdb {
    fn name(&self) -> &'static str {
        "tvdb"
    }

    fn search_shows(&self, title: &str, year: Option<i32>) -> Result<Vec<Candidate>> {
        let mut query = vec![("query", title.to_string()), ("type", "series".to_string())];
        if let Some(year) = year {
            query.push(("year", year.to_string()));
        }
        let mut found: Vec<SearchResult> = self.get("/search", &query)?.data;
        if found.is_empty() && year.is_some() {
            found = self.get("/search", &query[..2])?.data;
        }
        Ok(found
            .into_iter()
            .map(|result| {
                let translated = result
                    .translations
                    .and_then(|mut names| names.remove(&self.language))
                    .filter(|name| !name.is_empty());
                let translated_overview = result
                    .overviews
                    .and_then(|mut overviews| overviews.remove(&self.language));
                let title = translated.unwrap_or_else(|| result.name.clone());
                Candidate {
                    source: "tvdb",
                    url: series_url(&result.tvdb_id),
                    id: result.tvdb_id,
                    original_title: distinct_from(&title, Some(result.name)),
                    overview: non_empty(translated_overview).or(non_empty(result.overview)),
                    title,
                    year: result
                        .year
                        .and_then(|year| year.parse().ok())
                        .or_else(|| year_of(result.first_air_time.as_deref())),
                }
            })
            .collect())
    }

    fn show(&self, id: &str) -> Result<Candidate> {
        let series: Series = self.get(&format!("/series/{id}"), &[])?.data;
        // The base record carries the original-language name; prefer the configured language.
        let translation = self
            .get::<Translation>(&format!("/series/{id}/translations/{}", self.language), &[])
            .ok()
            .map(|translation| translation.data);
        let translated = translation
            .as_ref()
            .and_then(|translation| translation.name.clone())
            .filter(|name| !name.is_empty());
        let translated_overview = translation.and_then(|translation| translation.overview);
        let title = translated.unwrap_or_else(|| series.name.clone());
        Ok(Candidate {
            source: "tvdb",
            url: series_url(&series.id.to_string()),
            id: series.id.to_string(),
            original_title: distinct_from(&title, Some(series.name)),
            overview: non_empty(translated_overview).or(non_empty(series.overview)),
            title,
            year: series
                .year
                .and_then(|year| year.parse().ok())
                .or_else(|| year_of(series.first_aired.as_deref())),
        })
    }

    fn episodes(&self, id: &str, season: Option<u32>) -> Result<Vec<EpisodeInfo>> {
        let path = format!("/series/{id}/episodes/default/{}", self.language);
        let mut episodes = Vec::new();
        for page in 0.. {
            let mut query = vec![("page", page.to_string())];
            if let Some(season) = season {
                query.push(("season", season.to_string()));
            }
            let response: Envelope<EpisodePage> = self.get(&path, &query)?;
            let empty = response.data.episodes.is_empty();
            episodes.extend(response.data.episodes.into_iter().filter_map(|episode| {
                Some(EpisodeInfo {
                    season: episode.season?,
                    number: episode.number?,
                    title: episode.name.filter(|name| !name.is_empty()),
                    air_date: episode.aired.filter(|date| !date.is_empty()),
                })
            }));
            if empty || response.links.and_then(|links| links.next).is_none() {
                break;
            }
        }
        Ok(episodes)
    }
}
