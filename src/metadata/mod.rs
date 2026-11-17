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
    /// The candidate's page on the source's website.
    pub url: String,
    /// The title in its original language, when it differs from `title`.
    pub original_title: Option<String>,
    pub overview: Option<String>,
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
        .user_agent(concat!("reel/", env!("CARGO_PKG_VERSION")))
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

/// Returns `text` unless it is empty or the same as `title`.
pub(crate) fn distinct_from(title: &str, text: Option<String>) -> Option<String> {
    text.filter(|text| !text.is_empty() && text != title)
}

pub(crate) fn non_empty(text: Option<String>) -> Option<String> {
    text.filter(|text| !text.is_empty())
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

/// A minimal HTTP server on localhost, so the clients can be tested without the network.
#[cfg(test)]
pub(crate) mod testserver {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::thread;

    #[derive(Debug, Clone)]
    pub struct Request {
        pub method: String,
        pub target: String,
        pub headers: Vec<(String, String)>,
        pub body: String,
    }

    impl Request {
        pub fn path(&self) -> &str {
            self.target.split('?').next().unwrap_or_default()
        }

        /// The decoded value of a query parameter.
        pub fn param(&self, name: &str) -> Option<String> {
            let (_, query) = self.target.split_once('?')?;
            query.split('&').find_map(|pair| {
                let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                (key == name).then(|| decode(value))
            })
        }

        pub fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        }
    }

    fn decode(text: &str) -> String {
        let bytes = text.replace('+', " ").into_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            let hex = (bytes[i] == b'%')
                .then(|| bytes.get(i + 1..i + 3))
                .flatten()
                .and_then(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok());
            match hex {
                Some(byte) => {
                    out.push(byte);
                    i += 3;
                }
                None => {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    pub struct Server {
        pub base: String,
        requests: Arc<Mutex<Vec<Request>>>,
    }

    impl Server {
        /// Starts a server answering every request with the status and JSON body `handler` returns.
        pub fn start(handler: impl Fn(&Request) -> (u16, String) + Send + Sync + 'static) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let handler = Arc::new(handler);
            let log = Arc::clone(&requests);
            thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let (handler, log) = (Arc::clone(&handler), Arc::clone(&log));
                    thread::spawn(move || {
                        let _ = serve(stream, &*handler, &log);
                    });
                }
            });
            Server { base, requests }
        }

        pub fn requests(&self) -> Vec<Request> {
            self.requests.lock().unwrap().clone()
        }

        /// Paths (without query strings) requested so far, in order.
        pub fn paths(&self) -> Vec<String> {
            self.requests()
                .iter()
                .map(|request| request.path().to_string())
                .collect()
        }
    }

    fn serve(
        stream: TcpStream,
        handler: &dyn Fn(&Request) -> (u16, String),
        log: &Mutex<Vec<Request>>,
    ) -> std::io::Result<()> {
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let target = parts.next().unwrap_or_default().to_string();

        let mut headers = Vec::new();
        loop {
            line.clear();
            reader.read_line(&mut line)?;
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                headers.push((name.trim().to_string(), value.trim().to_string()));
            }
        }
        let length = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.parse().ok())
            .unwrap_or(0);
        let mut body = vec![0; length];
        reader.read_exact(&mut body)?;

        let request = Request {
            method,
            target,
            headers,
            body: String::from_utf8_lossy(&body).into_owned(),
        };
        let (status, payload) = handler(&request);
        log.lock().unwrap().push(request);

        let mut stream = stream;
        write!(
            stream,
            "HTTP/1.1 {status} Status\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
            payload.len()
        )?;
        stream.flush()
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
            url: String::new(),
            original_title: None,
            overview: None,
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

    #[test]
    fn dates_yield_years() {
        assert_eq!(year_of(Some("1982-06-25")), Some(1982));
        assert_eq!(year_of(Some("1982")), Some(1982));
        assert_eq!(year_of(Some("")), None);
        assert_eq!(year_of(Some("abcd-01-01")), None);
        assert_eq!(year_of(None), None);
    }

    #[test]
    fn optional_text_helpers_drop_empty_and_repeated_values() {
        assert_eq!(distinct_from("Heat", Some("Heat".into())), None);
        assert_eq!(distinct_from("Heat", Some(String::new())), None);
        assert_eq!(distinct_from("Heat", None), None);
        assert_eq!(
            distinct_from("Heat", Some("Heat (original)".into())).as_deref(),
            Some("Heat (original)")
        );
        assert_eq!(non_empty(Some(String::new())), None);
        assert_eq!(non_empty(Some("x".into())).as_deref(), Some("x"));
        assert_eq!(non_empty(None), None);
    }

    #[test]
    fn titles_are_normalised_before_comparison() {
        assert_eq!(normalise("Tom & Jerry"), "tom and jerry");
        assert_eq!(normalise("Marvel’s  Agents"), "marvels agents");
        assert_eq!(normalise("Face/Off: The Film!"), "face off the film");
        assert_eq!(normalise("  ÉCOLE  "), "école");
        assert_eq!(normalise(""), "");
    }

    #[test]
    fn year_differences_are_penalised_progressively() {
        let heat = candidate("Heat", 1995);
        let at = |year| distance("Heat", year, &heat);
        assert_eq!(at(Some(1995)), 0.0);
        assert!((at(Some(1996)) - 0.05).abs() < 1e-9);
        assert!((at(Some(1994)) - 0.05).abs() < 1e-9);
        assert!((at(Some(1990)) - 0.25).abs() < 1e-9);
        assert!((at(None) - 0.02).abs() < 1e-9);

        let mut undated = heat.clone();
        undated.year = None;
        assert!((distance("Heat", Some(1995), &undated) - 0.1).abs() < 1e-9);
    }

    #[test]
    fn distance_never_exceeds_one() {
        let c = candidate("Zzzzzz", 1950);
        assert_eq!(distance("Heat", Some(2020), &c), 1.0);
    }

    #[test]
    fn ranking_is_stable_among_ties() {
        let mut first = candidate("Heat", 1995);
        first.id = "first".into();
        let mut second = candidate("Heat", 1995);
        second.id = "second".into();
        let ranked = rank("Heat", Some(1995), vec![first, second]);
        let ids: Vec<_> = ranked.iter().map(|(_, c)| c.id.as_str()).collect();
        assert_eq!(ids, ["first", "second"]);
    }

    #[test]
    fn strength_needs_a_good_and_clearly_better_match() {
        let ranked = |distances: &[f64]| -> Vec<(f64, Candidate)> {
            distances
                .iter()
                .map(|&d| (d, candidate("x", 2000)))
                .collect()
        };
        assert!(!is_strong(&[], 0.08));
        assert!(is_strong(&ranked(&[0.0]), 0.08));
        assert!(
            is_strong(&ranked(&[0.08]), 0.08),
            "the threshold is inclusive"
        );
        assert!(!is_strong(&ranked(&[0.09]), 0.08));
        assert!(
            !is_strong(&ranked(&[0.0, 0.03]), 0.08),
            "too close to the runner-up"
        );
        assert!(is_strong(&ranked(&[0.0, 0.04]), 0.08));
        assert!(
            !is_strong(&ranked(&[0.1, 0.9]), 0.08),
            "ahead but not good enough"
        );
    }

    #[test]
    fn http_failures_become_helpful_errors() {
        let failure = |error| {
            read_json::<serde_json::Value>("TMDB", Err(error))
                .unwrap_err()
                .to_string()
        };
        assert_eq!(
            failure(ureq::Error::StatusCode(401)),
            "TMDB rejected the API key (HTTP 401)"
        );
        assert_eq!(
            failure(ureq::Error::StatusCode(404)),
            "TMDB: not found (HTTP 404)"
        );
        assert_eq!(failure(ureq::Error::StatusCode(500)), "TMDB request failed");
    }

    #[test]
    fn sources_are_chosen_from_the_configuration() {
        let mut config = MetadataConfig {
            tmdb_api_key: "tmdb".into(),
            tvdb_api_key: "tvdb".into(),
            ..MetadataConfig::default()
        };
        assert!(movie_source(&config).is_ok());
        assert_eq!(show_source(&config).unwrap().name(), "tvdb");
        config.tv_source = TvSource::Tmdb;
        assert_eq!(show_source(&config).unwrap().name(), "tmdb");
    }

    #[test]
    fn missing_keys_are_reported_by_service() {
        let mut config = MetadataConfig::default();
        let message = |result: Result<Box<dyn MovieSource>>| result.err().unwrap().to_string();
        assert!(message(movie_source(&config)).contains("TMDB API key"));
        assert!(
            show_source(&config)
                .err()
                .unwrap()
                .to_string()
                .contains("TVDB API key")
        );

        config.tv_source = TvSource::Tmdb;
        assert!(
            show_source(&config)
                .err()
                .unwrap()
                .to_string()
                .contains("TMDB API key")
        );
    }
}
