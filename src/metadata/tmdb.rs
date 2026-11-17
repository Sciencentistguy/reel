//! The Movie Database (API v3).

use anyhow::{Result, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::{
    Candidate, EpisodeInfo, MovieSource, ShowSource, agent, distinct_from, non_empty, read_json,
    year_of,
};
use crate::config::MetadataConfig;

const BASE: &str = "https://api.themoviedb.org/3";

pub struct Tmdb {
    agent: ureq::Agent,
    key: String,
    language: String,
}

#[derive(Deserialize)]
struct Page<T> {
    results: Vec<T>,
}

#[derive(Deserialize)]
struct Movie {
    id: u64,
    title: String,
    original_title: Option<String>,
    overview: Option<String>,
    release_date: Option<String>,
}

#[derive(Deserialize)]
struct Show {
    id: u64,
    name: String,
    original_name: Option<String>,
    overview: Option<String>,
    first_air_date: Option<String>,
    #[serde(default)]
    seasons: Vec<SeasonSummary>,
}

#[derive(Deserialize)]
struct SeasonSummary {
    season_number: u32,
}

#[derive(Deserialize)]
struct Season {
    #[serde(default)]
    episodes: Vec<Episode>,
}

#[derive(Deserialize)]
struct Episode {
    season_number: u32,
    episode_number: u32,
    name: Option<String>,
    air_date: Option<String>,
}

impl From<Movie> for Candidate {
    fn from(movie: Movie) -> Self {
        Candidate {
            source: "tmdb",
            id: movie.id.to_string(),
            url: format!("https://www.themoviedb.org/movie/{}", movie.id),
            year: year_of(movie.release_date.as_deref()),
            original_title: distinct_from(&movie.title, movie.original_title),
            overview: non_empty(movie.overview),
            title: movie.title,
        }
    }
}

impl From<Show> for Candidate {
    fn from(show: Show) -> Self {
        Candidate {
            source: "tmdb",
            id: show.id.to_string(),
            url: format!("https://www.themoviedb.org/tv/{}", show.id),
            year: year_of(show.first_air_date.as_deref()),
            original_title: distinct_from(&show.name, show.original_name),
            overview: non_empty(show.overview),
            title: show.name,
        }
    }
}

impl Tmdb {
    pub fn new(config: &MetadataConfig) -> Result<Self> {
        if config.tmdb_api_key.is_empty() {
            bail!(
                "no TMDB API key configured: set metadata.tmdb_api_key or TMDB_API_KEY \
                 (free keys: https://www.themoviedb.org/settings/api), or import with --asis"
            );
        }
        Ok(Tmdb {
            agent: agent(),
            key: config.tmdb_api_key.clone(),
            language: config.tmdb_language.clone(),
        })
    }

    fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, String)]) -> Result<T> {
        let mut request = self
            .agent
            .get(format!("{BASE}{path}"))
            .query("language", &self.language);
        // v4 read access tokens are JWTs sent as bearer tokens; v3 keys go in the query string.
        request = if self.key.len() > 40 {
            request.header("Authorization", format!("Bearer {}", self.key))
        } else {
            request.query("api_key", &self.key)
        };
        for (name, value) in query {
            request = request.query(*name, value);
        }
        read_json("TMDB", request.call())
    }

    fn search<T: DeserializeOwned + Into<Candidate>>(
        &self,
        path: &str,
        year_parameter: &str,
        title: &str,
        year: Option<i32>,
    ) -> Result<Vec<Candidate>> {
        let mut query = vec![("query", title.to_string())];
        if let Some(year) = year {
            query.push((year_parameter, year.to_string()));
        }
        let mut page: Page<T> = self.get(path, &query)?;
        if page.results.is_empty() && year.is_some() {
            page = self.get(path, &query[..1])?;
        }
        Ok(page.results.into_iter().map(Into::into).collect())
    }

    fn season(&self, id: &str, season: u32) -> Result<Vec<EpisodeInfo>> {
        let season: Season = self.get(&format!("/tv/{id}/season/{season}"), &[])?;
        Ok(season
            .episodes
            .into_iter()
            .map(|episode| EpisodeInfo {
                season: episode.season_number,
                number: episode.episode_number,
                title: episode.name.filter(|name| !name.is_empty()),
                air_date: episode.air_date.filter(|date| !date.is_empty()),
            })
            .collect())
    }
}

impl MovieSource for Tmdb {
    fn search_movies(&self, title: &str, year: Option<i32>) -> Result<Vec<Candidate>> {
        self.search::<Movie>("/search/movie", "year", title, year)
    }

    fn movie(&self, id: &str) -> Result<Candidate> {
        Ok(self.get::<Movie>(&format!("/movie/{id}"), &[])?.into())
    }
}

impl ShowSource for Tmdb {
    fn name(&self) -> &'static str {
        "tmdb"
    }

    fn search_shows(&self, title: &str, year: Option<i32>) -> Result<Vec<Candidate>> {
        self.search::<Show>("/search/tv", "first_air_date_year", title, year)
    }

    fn show(&self, id: &str) -> Result<Candidate> {
        Ok(self.get::<Show>(&format!("/tv/{id}"), &[])?.into())
    }

    fn episodes(&self, id: &str, season: Option<u32>) -> Result<Vec<EpisodeInfo>> {
        if let Some(season) = season {
            return self.season(id, season);
        }
        let show: Show = self.get(&format!("/tv/{id}"), &[])?;
        let mut episodes = Vec::new();
        for summary in show.seasons {
            episodes.extend(self.season(id, summary.season_number)?);
        }
        Ok(episodes)
    }
}
