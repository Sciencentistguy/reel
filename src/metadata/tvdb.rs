//! TheTVDB (API v4).

use std::cell::RefCell;
use std::collections::HashMap;

use anyhow::{Result, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::{Candidate, EpisodeInfo, Http, ShowSource, distinct_from, non_empty, year_of};
use crate::config::MetadataConfig;

const BASE: &str = "https://api4.thetvdb.com/v4";

pub struct Tvdb {
    base: String,
    http: Http,
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

#[derive(Debug, PartialEq)]
enum Entered<'a> {
    Id(&'a str),
    Slug(&'a str),
}

/// Accepts a bare ID, a slug such as `archer-2009`, or a TVDB page URL.
fn entered_id(input: &str) -> Entered<'_> {
    let input = input.trim();
    let (path, query) = input.split_once('?').unwrap_or((input, ""));
    let query = query.split('#').next().unwrap_or_default();
    let path = path.split('#').next().unwrap_or_default();
    let from_query = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("id="))
        .filter(|id| !id.is_empty());
    let segment = from_query.unwrap_or_else(|| {
        if path.contains('/') {
            let mut segments = path.split('/');
            segments.find(|segment| *segment == "series");
            segments.next().unwrap_or(path)
        } else {
            path
        }
    });
    if !segment.is_empty() && segment.bytes().all(|byte| byte.is_ascii_digit()) {
        Entered::Id(segment)
    } else {
        Entered::Slug(segment)
    }
}

/// TVDB disambiguates names with the year, as in `Archer (2009)`; the year is kept separately.
fn without_year(name: String, year: Option<i32>) -> String {
    let Some(year) = year else { return name };
    match name.strip_suffix(&format!(" ({year})")) {
        Some(stripped) if !stripped.trim().is_empty() => stripped.to_string(),
        _ => name,
    }
}

impl Tvdb {
    pub fn new(config: &MetadataConfig) -> Result<Self> {
        Self::with_base(config, BASE)
    }

    fn with_base(config: &MetadataConfig, base: &str) -> Result<Self> {
        if config.tvdb_api_key.is_empty() {
            bail!(
                "no TVDB API key configured: set metadata.tvdb_api_key or TVDB_API_KEY \
                 (https://thetvdb.com/api-information), or set metadata.tv_source = \"tmdb\""
            );
        }
        Ok(Tvdb {
            base: base.to_string(),
            http: Http::new("TVDB"),
            key: config.tvdb_api_key.clone(),
            language: config.tvdb_language.clone(),
            token: RefCell::new(None),
        })
    }

    fn token(&self) -> Result<String> {
        if let Some(token) = self.token.borrow().as_ref() {
            return Ok(token.clone());
        }
        let login: Envelope<Login> = self.http.json(|agent| {
            agent
                .post(format!("{}/login", self.base))
                .send_json(serde_json::json!({ "apikey": self.key }))
        })?;
        *self.token.borrow_mut() = Some(login.data.token.clone());
        Ok(login.data.token)
    }

    fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<Envelope<T>> {
        let token = self.token()?;
        self.http.json(|agent| {
            let mut request = agent
                .get(format!("{}{path}", self.base))
                .header("Authorization", format!("Bearer {token}"));
            for (name, value) in query {
                request = request.query(*name, value);
            }
            request.call()
        })
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
                let year = result
                    .year
                    .and_then(|year| year.parse().ok())
                    .or_else(|| year_of(result.first_air_time.as_deref()));
                let name = without_year(result.name, year);
                let title = translated.map_or_else(|| name.clone(), |t| without_year(t, year));
                Candidate {
                    source: "tvdb",
                    url: series_url(&result.tvdb_id),
                    id: result.tvdb_id,
                    original_title: distinct_from(&title, Some(name)),
                    overview: non_empty(translated_overview).or(non_empty(result.overview)),
                    title,
                    year,
                    runtime: None,
                    episodes: None,
                }
            })
            .collect())
    }

    fn show(&self, id: &str) -> Result<Candidate> {
        let path = match entered_id(id) {
            Entered::Id(id) => format!("/series/{id}"),
            Entered::Slug(slug) => format!("/series/slug/{slug}"),
        };
        let series: Series = self.get(&path, &[])?.data;
        let id = series.id;
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
        let year = series
            .year
            .and_then(|year| year.parse().ok())
            .or_else(|| year_of(series.first_aired.as_deref()));
        let name = without_year(series.name, year);
        let title = translated.map_or_else(|| name.clone(), |t| without_year(t, year));
        Ok(Candidate {
            source: "tvdb",
            url: series_url(&id.to_string()),
            id: id.to_string(),
            original_title: distinct_from(&title, Some(name)),
            overview: non_empty(translated_overview).or(non_empty(series.overview)),
            title,
            year,
            runtime: None,
            episodes: None,
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use super::*;
    use crate::metadata::testserver::{Request, Server};

    fn client(server: &Server) -> Tvdb {
        let config = MetadataConfig {
            tvdb_api_key: "secret-key".into(),
            ..MetadataConfig::default()
        };
        let mut tvdb = Tvdb::with_base(&config, &server.base).unwrap();
        tvdb.http.retry.backoff = Duration::ZERO;
        tvdb
    }

    fn ok(body: &str) -> (u16, String) {
        (200, body.to_string())
    }

    fn login() -> (u16, String) {
        ok(r#"{"data": {"token": "tok"}}"#)
    }

    const SEARCH: &str = r#"{"data": [
        {"tvdb_id": "78804", "name": "Doctor Who", "year": "2005",
         "first_air_time": "2005-03-26", "overview": "Original overview",
         "translations": {"eng": "Doctor Who (English)", "fra": "Le Docteur"},
         "overviews": {"eng": "English overview"}},
        {"tvdb_id": "2", "name": "Plain Show", "first_air_time": "1999-09-09",
         "overview": "Plain overview"},
        {"tvdb_id": "3", "name": "Blank", "translations": {"eng": ""},
         "overviews": {"eng": ""}, "overview": "Fallback"}
    ]}"#;

    fn search_server() -> Server {
        Server::start(|request: &Request| match request.path() {
            "/login" => login(),
            "/search" => ok(SEARCH),
            _ => (404, "{}".into()),
        })
    }

    #[test]
    fn a_key_is_required() {
        let error = Tvdb::new(&MetadataConfig::default()).err().unwrap();
        assert!(error.to_string().contains("no TVDB API key"));
    }

    #[test]
    fn rate_limited_logins_and_requests_are_retried() {
        let (login_limited, search_limited) = (AtomicBool::new(true), AtomicBool::new(true));
        let server = Server::start(move |request: &Request| {
            let limited = match request.path() {
                "/login" => &login_limited,
                _ => &search_limited,
            };
            match (limited.swap(false, Ordering::SeqCst), request.path()) {
                (true, _) => (429, "{}".into()),
                (false, "/login") => login(),
                (false, _) => ok(SEARCH),
            }
        });
        assert_eq!(
            client(&server)
                .search_shows("Doctor Who", None)
                .unwrap()
                .len(),
            3
        );
        assert_eq!(server.paths(), ["/login", "/login", "/search", "/search"]);
    }

    #[test]
    fn searches_log_in_once_and_reuse_the_token() {
        let server = search_server();
        let tvdb = client(&server);
        tvdb.search_shows("Doctor Who", None).unwrap();
        tvdb.search_shows("Plain Show", None).unwrap();

        let requests = server.requests();
        let logins: Vec<_> = requests.iter().filter(|r| r.path() == "/login").collect();
        assert_eq!(logins.len(), 1);
        assert_eq!(logins[0].method, "POST");
        assert!(logins[0].body.contains("secret-key"));
        for search in requests.iter().filter(|r| r.path() == "/search") {
            assert_eq!(search.header("authorization").unwrap(), "Bearer tok");
        }
        assert_eq!(requests.len(), 3);
    }

    #[test]
    fn a_rejected_key_is_reported() {
        let server = Server::start(|_| (401, "{}".into()));
        let error = client(&server).search_shows("x", None).unwrap_err();
        assert!(error.to_string().contains("rejected the API key"));
    }

    #[test]
    fn search_results_prefer_the_configured_language() {
        let server = search_server();
        let found = client(&server).search_shows("Doctor Who", None).unwrap();
        assert_eq!(found.len(), 3);

        assert_eq!(
            found[0],
            Candidate {
                source: "tvdb",
                id: "78804".into(),
                title: "Doctor Who (English)".into(),
                year: Some(2005),
                url: "https://thetvdb.com/?tab=series&id=78804".into(),
                original_title: Some("Doctor Who".into()),
                overview: Some("English overview".into()),
                runtime: None,
                episodes: None,
            }
        );
        assert_eq!(found[1].title, "Plain Show");
        assert_eq!(found[1].year, Some(1999), "from the first air time");
        assert_eq!(found[1].original_title, None);
        assert_eq!(found[1].overview.as_deref(), Some("Plain overview"));
        assert_eq!(found[2].title, "Blank", "an empty translation is ignored");
        assert_eq!(found[2].overview.as_deref(), Some("Fallback"));
    }

    #[test]
    fn search_asks_for_series_with_the_year() {
        let server = search_server();
        client(&server)
            .search_shows("Doctor Who", Some(2005))
            .unwrap();
        let search = &server.requests()[1];
        assert_eq!(search.param("query").unwrap(), "Doctor Who");
        assert_eq!(search.param("type").unwrap(), "series");
        assert_eq!(search.param("year").unwrap(), "2005");
    }

    #[test]
    fn an_empty_dated_search_is_retried_without_the_year() {
        let server = Server::start(|request: &Request| match request.path() {
            "/login" => login(),
            _ if request.param("year").is_some() => ok(r#"{"data": []}"#),
            _ => ok(SEARCH),
        });
        let found = client(&server)
            .search_shows("Doctor Who", Some(1990))
            .unwrap();
        assert_eq!(found.len(), 3);

        let searches: Vec<_> = server
            .requests()
            .into_iter()
            .filter(|r| r.path() == "/search")
            .collect();
        assert_eq!(searches.len(), 2);
        assert!(searches[1].param("year").is_none());
        assert_eq!(searches[1].param("type").unwrap(), "series");
    }

    fn show_server(translation: (u16, &'static str)) -> Server {
        Server::start(move |request: &Request| match request.path() {
            "/login" => login(),
            "/series/1" => ok(
                r#"{"data": {"id": 1, "name": "Original Name", "year": "2005",
                "overview": "Original overview"}}"#,
            ),
            "/series/1/translations/eng" => (translation.0, translation.1.to_string()),
            _ => (404, "{}".into()),
        })
    }

    #[test]
    fn a_show_takes_its_translated_name_and_overview() {
        let server = show_server((
            200,
            r#"{"data": {"name": "Translated Name", "overview": "Translated overview"}}"#,
        ));
        let show = client(&server).show("1").unwrap();
        assert_eq!(show.title, "Translated Name");
        assert_eq!(show.original_title.as_deref(), Some("Original Name"));
        assert_eq!(show.overview.as_deref(), Some("Translated overview"));
        assert_eq!(show.year, Some(2005));
        assert_eq!(show.url, "https://thetvdb.com/?tab=series&id=1");
        assert_eq!(client(&server).name(), "tvdb");
    }

    #[test]
    fn a_show_without_a_translation_keeps_its_original_details() {
        let server = show_server((404, "{}"));
        let show = client(&server).show("1").unwrap();
        assert_eq!(show.title, "Original Name");
        assert_eq!(show.original_title, None);
        assert_eq!(show.overview.as_deref(), Some("Original overview"));
    }

    #[test]
    fn entered_ids_accept_slugs_and_page_urls() {
        for input in [
            "110381",
            " 110381 ",
            "https://thetvdb.com/?tab=series&id=110381",
            "https://thetvdb.com/dereferrer/series/110381",
        ] {
            assert_eq!(entered_id(input), Entered::Id("110381"), "{input}");
        }
        for input in [
            "archer-2009",
            "https://thetvdb.com/series/archer-2009",
            "https://thetvdb.com/series/archer-2009/seasons/official/4",
            "thetvdb.com/series/archer-2009#overview",
        ] {
            assert_eq!(entered_id(input), Entered::Slug("archer-2009"), "{input}");
        }
    }

    #[test]
    fn a_show_can_be_found_by_slug_and_loses_its_disambiguating_year() {
        let server = Server::start(|request: &Request| match request.path() {
            "/login" => login(),
            "/series/slug/archer-2009" => ok(
                r#"{"data": {"id": 110381, "name": "Archer (2009)", "year": "2009",
                "overview": "Spies"}}"#,
            ),
            "/series/110381/translations/eng" => ok(r#"{"data": {"name": "Archer (2009)"}}"#),
            _ => (404, "{}".into()),
        });
        let show = client(&server)
            .show("https://thetvdb.com/series/archer-2009")
            .unwrap();
        assert_eq!(
            (show.id.as_str(), show.title.as_str()),
            ("110381", "Archer")
        );
        assert_eq!((show.year, show.original_title), (Some(2009), None));
    }

    #[test]
    fn only_a_matching_year_is_removed_from_names() {
        assert_eq!(without_year("Archer (2009)".into(), Some(2009)), "Archer");
        assert_eq!(
            without_year("Archer (2009)".into(), Some(2010)),
            "Archer (2009)"
        );
        assert_eq!(
            without_year("The Office (US)".into(), Some(2005)),
            "The Office (US)"
        );
        assert_eq!(without_year("(2009)".into(), Some(2009)), "(2009)");
        assert_eq!(without_year("Archer".into(), None), "Archer");
    }

    #[test]
    fn an_unknown_show_is_an_error() {
        let server = show_server((404, "{}"));
        assert!(client(&server).show("999").is_err());
    }

    fn episode_server() -> Server {
        Server::start(|request: &Request| match request.path() {
            "/login" => login(),
            "/series/1/episodes/default/eng" => match request.param("page").as_deref() {
                Some("0") => ok(r#"{"data": {"episodes": [
                        {"seasonNumber": 1, "number": 1, "name": "One", "aired": "2005-01-01"},
                        {"seasonNumber": 1, "number": null, "name": "Unnumbered"},
                        {"number": 9, "name": "Seasonless"}]},
                    "links": {"next": "https://example.invalid/?page=1"}}"#),
                Some("1") => ok(r#"{"data": {"episodes": [
                        {"seasonNumber": 1, "number": 2, "name": "", "aired": ""}]},
                    "links": {"next": null}}"#),
                _ => ok(r#"{"data": {"episodes": []}}"#),
            },
            "/series/2/episodes/default/eng" => ok(
                r#"{"data": {"episodes": []}, "links": {"next": "https://example.invalid/?page=1"}}"#,
            ),
            "/series/3/episodes/default/eng" => {
                ok(r#"{"data": {"episodes": [{"seasonNumber": 1, "number": 1}]}}"#)
            }
            _ => (404, "{}".into()),
        })
    }

    #[test]
    fn episodes_follow_pagination_and_skip_unplaceable_entries() {
        let server = episode_server();
        let episodes = client(&server).episodes("1", None).unwrap();
        assert_eq!(
            episodes,
            [
                EpisodeInfo {
                    season: 1,
                    number: 1,
                    title: Some("One".into()),
                    air_date: Some("2005-01-01".into()),
                },
                EpisodeInfo {
                    season: 1,
                    number: 2,
                    title: None,
                    air_date: None,
                },
            ]
        );
        let pages: Vec<_> = server
            .requests()
            .iter()
            .filter(|r| r.path().starts_with("/series"))
            .map(|r| r.param("page").unwrap())
            .collect();
        assert_eq!(pages, ["0", "1"]);
    }

    #[test]
    fn a_season_filter_is_sent_with_every_page() {
        let server = episode_server();
        client(&server).episodes("1", Some(2)).unwrap();
        let requests = server.requests();
        let pages: Vec<_> = requests
            .iter()
            .filter(|r| r.path().starts_with("/series"))
            .collect();
        assert_eq!(pages.len(), 2);
        assert!(pages.iter().all(|r| r.param("season").unwrap() == "2"));
    }

    #[test]
    fn no_season_filter_is_sent_by_default() {
        let server = episode_server();
        client(&server).episodes("1", None).unwrap();
        assert!(
            server
                .requests()
                .iter()
                .all(|request| request.param("season").is_none())
        );
    }

    #[test]
    fn an_empty_page_ends_pagination_even_if_more_is_promised() {
        let server = episode_server();
        assert!(client(&server).episodes("2", None).unwrap().is_empty());
        let pages = server
            .requests()
            .iter()
            .filter(|r| r.path().starts_with("/series"))
            .count();
        assert_eq!(pages, 1);
    }

    #[test]
    fn a_missing_next_link_ends_pagination() {
        let server = episode_server();
        assert_eq!(client(&server).episodes("3", None).unwrap().len(), 1);
        let pages = server
            .requests()
            .iter()
            .filter(|r| r.path().starts_with("/series"))
            .count();
        assert_eq!(pages, 1);
    }
}
