//! The Movie Database (API v3).

use anyhow::{Result, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::{
    Candidate, EpisodeInfo, Http, MovieSource, ShowSource, distinct_from, non_empty, year_of,
};
use crate::config::MetadataConfig;

const BASE: &str = "https://api.themoviedb.org/3";

pub struct Tmdb {
    base: String,
    http: Http,
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
        Self::with_base(config, BASE)
    }

    fn with_base(config: &MetadataConfig, base: &str) -> Result<Self> {
        if config.tmdb_api_key.is_empty() {
            bail!(
                "no TMDB API key configured: set metadata.tmdb_api_key or TMDB_API_KEY \
                 (free keys: https://www.themoviedb.org/settings/api), or import with --asis"
            );
        }
        Ok(Tmdb {
            base: base.to_string(),
            http: Http::new("TMDB"),
            key: config.tmdb_api_key.clone(),
            language: config.tmdb_language.clone(),
        })
    }

    fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, String)]) -> Result<T> {
        self.http.json(|agent| {
            let mut request = agent
                .get(format!("{}{path}", self.base))
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
            request.call()
        })
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use super::*;
    use crate::metadata::testserver::{Request, Server};

    fn client(server: &Server, key: &str) -> Tmdb {
        let config = MetadataConfig {
            tmdb_api_key: key.into(),
            ..MetadataConfig::default()
        };
        let mut tmdb = Tmdb::with_base(&config, &server.base).unwrap();
        tmdb.http.retry.backoff = Duration::ZERO;
        tmdb
    }

    fn ok(body: &str) -> (u16, String) {
        (200, body.to_string())
    }

    const MOVIES: &str = r#"{"results": [
        {"id": 78, "title": "Blade Runner", "original_title": "Blade Runner",
         "overview": "", "release_date": "1982-06-25"},
        {"id": 1, "title": "The Film", "original_title": "Der Film",
         "overview": "A plot.", "release_date": ""},
        {"id": 2, "title": "Bare"}
    ]}"#;

    #[test]
    fn a_key_is_required() {
        let error = Tmdb::new(&MetadataConfig::default()).err().unwrap();
        assert!(error.to_string().contains("no TMDB API key"));
    }

    #[test]
    fn movie_searches_become_candidates() {
        let server = Server::start(|_| ok(MOVIES));
        let found = client(&server, "key")
            .search_movies("Blade Runner", Some(1982))
            .unwrap();

        assert_eq!(found.len(), 3);
        assert_eq!(
            found[0],
            Candidate {
                source: "tmdb",
                id: "78".into(),
                title: "Blade Runner".into(),
                year: Some(1982),
                url: "https://www.themoviedb.org/movie/78".into(),
                original_title: None,
                overview: None,
            }
        );
        assert_eq!(found[1].year, None);
        assert_eq!(found[1].original_title.as_deref(), Some("Der Film"));
        assert_eq!(found[1].overview.as_deref(), Some("A plot."));
        assert_eq!(found[2].title, "Bare");
    }

    #[test]
    fn searches_send_the_title_year_and_language() {
        let server = Server::start(|_| ok(MOVIES));
        client(&server, "key")
            .search_movies("Blade Runner & Co", Some(1982))
            .unwrap();
        let request = &server.requests()[0];
        assert_eq!(request.path(), "/search/movie");
        assert_eq!(request.param("query").unwrap(), "Blade Runner & Co");
        assert_eq!(request.param("year").unwrap(), "1982");
        assert_eq!(request.param("language").unwrap(), "en-US");
    }

    #[test]
    fn show_searches_use_the_first_air_date_year() {
        let server = Server::start(|_| ok(r#"{"results": []}"#));
        client(&server, "key")
            .search_shows("Doctor Who", Some(2005))
            .unwrap();
        let request = &server.requests()[0];
        assert_eq!(request.path(), "/search/tv");
        assert_eq!(request.param("first_air_date_year").unwrap(), "2005");
    }

    #[test]
    fn an_empty_dated_search_is_retried_without_the_year() {
        let server = Server::start(|request: &Request| {
            if request.param("year").is_some() {
                ok(r#"{"results": []}"#)
            } else {
                ok(MOVIES)
            }
        });
        let found = client(&server, "key")
            .search_movies("Blade Runner", Some(1990))
            .unwrap();
        assert_eq!(found.len(), 3);
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].param("year").is_some());
        assert!(requests[1].param("year").is_none());
    }

    #[test]
    fn searches_that_find_something_are_not_repeated() {
        let server = Server::start(|_| ok(MOVIES));
        client(&server, "key")
            .search_movies("Blade Runner", Some(1982))
            .unwrap();
        assert_eq!(server.requests().len(), 1);
    }

    #[test]
    fn undated_searches_are_not_repeated() {
        let server = Server::start(|_| ok(r#"{"results": []}"#));
        let found = client(&server, "key")
            .search_movies("Nothing", None)
            .unwrap();
        assert!(found.is_empty());
        assert_eq!(server.requests().len(), 1);
    }

    #[test]
    fn v3_keys_travel_in_the_query_string() {
        let server = Server::start(|_| ok(MOVIES));
        client(&server, "short-v3-key")
            .search_movies("x", None)
            .unwrap();
        let request = &server.requests()[0];
        assert_eq!(request.param("api_key").unwrap(), "short-v3-key");
        assert!(request.header("authorization").is_none());
    }

    #[test]
    fn v4_tokens_travel_as_bearer_credentials() {
        let token = "e".repeat(60);
        let server = Server::start(|_| ok(MOVIES));
        client(&server, &token).search_movies("x", None).unwrap();
        let request = &server.requests()[0];
        assert_eq!(
            request.header("authorization").unwrap(),
            format!("Bearer {token}")
        );
        assert!(request.param("api_key").is_none());
    }

    #[test]
    fn the_configured_language_is_requested() {
        let server = Server::start(|_| ok(MOVIES));
        let config = MetadataConfig {
            tmdb_api_key: "key".into(),
            tmdb_language: "fr-FR".into(),
            ..MetadataConfig::default()
        };
        Tmdb::with_base(&config, &server.base)
            .unwrap()
            .search_movies("x", None)
            .unwrap();
        assert_eq!(server.requests()[0].param("language").unwrap(), "fr-FR");
    }

    #[test]
    fn lookups_by_id_fetch_one_record() {
        let server = Server::start(|request: &Request| match request.path() {
            "/movie/78" => {
                ok(r#"{"id": 78, "title": "Blade Runner", "release_date": "1982-06-25"}"#)
            }
            "/tv/1396" => ok(
                r#"{"id": 1396, "name": "Breaking Bad", "original_name": "Breaking Bad",
                    "first_air_date": "2008-01-20", "overview": "Chemistry."}"#,
            ),
            _ => (404, "{}".into()),
        });
        let tmdb = client(&server, "key");

        let film = tmdb.movie("78").unwrap();
        assert_eq!(
            (film.title.as_str(), film.year),
            ("Blade Runner", Some(1982))
        );

        let show = tmdb.show("1396").unwrap();
        assert_eq!(show.url, "https://www.themoviedb.org/tv/1396");
        assert_eq!(show.year, Some(2008));
        assert_eq!(show.overview.as_deref(), Some("Chemistry."));
        assert_eq!(show.original_title, None);
        assert_eq!(tmdb.name(), "tmdb");
    }

    #[test]
    fn rate_limited_requests_are_retried() {
        let limited = AtomicBool::new(true);
        let server = Server::start(move |_| {
            if limited.swap(false, Ordering::SeqCst) {
                (429, "{}".into())
            } else {
                ok(r#"{"id": 78, "title": "Blade Runner"}"#)
            }
        });
        assert_eq!(
            client(&server, "key").movie("78").unwrap().title,
            "Blade Runner"
        );
        assert_eq!(server.paths(), ["/movie/78", "/movie/78"]);
    }

    #[test]
    fn http_errors_are_explained() {
        for (status, expected) in [
            (401, "rejected the API key"),
            (404, "not found"),
            (500, "request failed"),
        ] {
            let server = Server::start(move |_| (status, "{}".into()));
            let error = client(&server, "key").movie("1").unwrap_err();
            assert!(error.to_string().contains(expected), "{status}: {error}");
        }
    }

    #[test]
    fn malformed_responses_are_reported() {
        let server = Server::start(|_| ok("this is not json"));
        let error = client(&server, "key").movie("1").unwrap_err();
        assert!(error.to_string().contains("unexpected response from TMDB"));
    }

    fn series_server() -> Server {
        Server::start(|request: &Request| match request.path() {
            "/tv/1" => ok(r#"{"id": 1, "name": "Show",
                "seasons": [{"season_number": 0}, {"season_number": 1}]}"#),
            "/tv/1/season/0" => ok(r#"{"episodes": [
                {"season_number": 0, "episode_number": 1, "name": "Special",
                 "air_date": "2009-01-01"}]}"#),
            "/tv/1/season/1" => ok(r#"{"episodes": [
                {"season_number": 1, "episode_number": 1, "name": "", "air_date": null},
                {"season_number": 1, "episode_number": 2, "name": "Two", "air_date": ""}]}"#),
            "/tv/2" => ok(r#"{"id": 2, "name": "Bare"}"#),
            _ => (404, "{}".into()),
        })
    }

    #[test]
    fn all_episodes_are_gathered_season_by_season() {
        let server = series_server();
        let episodes = client(&server, "key").episodes("1", None).unwrap();
        assert_eq!(
            server.paths(),
            ["/tv/1", "/tv/1/season/0", "/tv/1/season/1"]
        );
        assert_eq!(
            episodes,
            [
                EpisodeInfo {
                    season: 0,
                    number: 1,
                    title: Some("Special".into()),
                    air_date: Some("2009-01-01".into()),
                },
                EpisodeInfo {
                    season: 1,
                    number: 1,
                    title: None,
                    air_date: None,
                },
                EpisodeInfo {
                    season: 1,
                    number: 2,
                    title: Some("Two".into()),
                    air_date: None,
                },
            ]
        );
    }

    #[test]
    fn one_season_needs_one_request() {
        let server = series_server();
        let episodes = client(&server, "key").episodes("1", Some(1)).unwrap();
        assert_eq!(server.paths(), ["/tv/1/season/1"]);
        assert_eq!(episodes.len(), 2);
    }

    #[test]
    fn a_show_without_seasons_has_no_episodes() {
        let server = series_server();
        assert!(
            client(&server, "key")
                .episodes("2", None)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_failing_season_fails_the_whole_listing() {
        let server = series_server();
        assert!(client(&server, "key").episodes("1", Some(9)).is_err());
    }
}
