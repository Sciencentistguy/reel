//! Online metadata sources and candidate scoring.

use std::fmt;
use std::thread;
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
    /// Running time in minutes; search results usually leave it out.
    pub runtime: Option<u32>,
    /// A show's full episode list, once fetched; searches leave it out.
    pub episodes: Option<Vec<EpisodeInfo>>,
}

/// A show's episode count, with specials (season 0) counted apart from the regular episodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpisodeCount {
    pub episodes: usize,
    pub specials: usize,
}

impl EpisodeCount {
    pub fn of(list: &[EpisodeInfo]) -> Self {
        let specials = list.iter().filter(|episode| episode.season == 0).count();
        EpisodeCount {
            episodes: list.len() - specials,
            specials,
        }
    }
}

impl fmt::Display for EpisodeCount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let plural = |count: usize, noun: &str| match count {
            1 => format!("1 {noun}"),
            count => format!("{count} {noun}s"),
        };
        f.write_str(&plural(self.episodes, "episode"))?;
        if self.specials > 0 {
            write!(f, ", {}", plural(self.specials, "special"))?;
        }
        Ok(())
    }
}

/// A file's running time, to compare with a candidate's.
#[derive(Debug, Clone, Copy)]
pub struct Length {
    pub seconds: f64,
    /// Whether the file is a cut other than the theatrical one, so may differ legitimately.
    pub alternate_cut: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EpisodeInfo {
    pub season: u32,
    pub number: u32,
    pub title: Option<String>,
    pub air_date: Option<String>,
    /// Running time in minutes, as the source rounds it.
    pub runtime: Option<u32>,
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

/// The show source called `name`, regardless of which one the configuration prefers.
pub fn show_source_named(config: &MetadataConfig, name: &str) -> Result<Box<dyn ShowSource>> {
    Ok(match name {
        "tvdb" => Box::new(tvdb::Tvdb::new(config)?),
        "tmdb" => Box::new(tmdb::Tmdb::new(config)?),
        _ => bail!("unknown metadata source {name:?}"),
    })
}

type Response = Result<ureq::http::Response<ureq::Body>, ureq::Error>;

/// Statuses that mean the service is rate limiting or briefly unavailable.
const TRANSIENT_STATUSES: [u16; 4] = [429, 502, 503, 504];

/// How requests are retried when a service is rate limiting or briefly unavailable.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Retry {
    /// Total attempts per request, including the first.
    pub attempts: u32,
    /// The wait before the first retry, doubled for each one after it.
    pub backoff: Duration,
    /// The longest single wait, including one the service asks for with `Retry-After`.
    pub max_wait: Duration,
}

impl Default for Retry {
    fn default() -> Self {
        Retry {
            attempts: 5,
            backoff: Duration::from_secs(1),
            max_wait: Duration::from_secs(60),
        }
    }
}

impl Retry {
    /// How long to wait before retrying after `result`, and why, or `None` when it shouldn't be retried.
    fn wait(&self, retry: u32, result: &Response) -> Option<(Duration, String)> {
        let (reason, requested) = match result {
            Ok(response) if TRANSIENT_STATUSES.contains(&response.status().as_u16()) => (
                format!("HTTP {}", response.status().as_u16()),
                retry_after(response),
            ),
            Err(err @ (ureq::Error::Io(_) | ureq::Error::ConnectionFailed)) => {
                (err.to_string(), None)
            }
            _ => return None,
        };
        let backoff = self.backoff.saturating_mul(2u32.saturating_pow(retry - 1));
        Some((requested.unwrap_or(backoff).min(self.max_wait), reason))
    }
}

/// The delay a `Retry-After` header asks for, when given in seconds.
fn retry_after(response: &ureq::http::Response<ureq::Body>) -> Option<Duration> {
    let seconds = response.headers().get("retry-after")?.to_str().ok()?;
    seconds.trim().parse().ok().map(Duration::from_secs)
}

/// An HTTP client for one metadata service.
pub(crate) struct Http {
    service: &'static str,
    agent: ureq::Agent,
    pub retry: Retry,
}

impl Http {
    pub fn new(service: &'static str) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            .user_agent(concat!("reel/", env!("CARGO_PKG_VERSION")))
            .http_status_as_error(false)
            .build()
            .into();
        Http {
            service,
            agent,
            retry: Retry::default(),
        }
    }

    /// Sends the request `send` makes and decodes its JSON response, retrying it while the
    /// service is rate limiting or briefly unavailable.
    pub fn json<T: DeserializeOwned>(&self, send: impl Fn(&ureq::Agent) -> Response) -> Result<T> {
        let mut retry = 1;
        loop {
            let result = send(&self.agent);
            match self.retry.wait(retry, &result) {
                Some((wait, reason)) if retry < self.retry.attempts => {
                    eprintln!(
                        "  {} is unavailable ({reason}), retrying in {}s",
                        self.service,
                        wait.as_secs_f32().ceil()
                    );
                    thread::sleep(wait);
                    retry += 1;
                }
                _ => return read_json(self.service, result),
            }
        }
    }
}

fn read_json<T: DeserializeOwned>(service: &str, result: Response) -> Result<T> {
    let mut response = result.with_context(|| format!("{service} request failed"))?;
    match response.status().as_u16() {
        200..=299 => response
            .body_mut()
            .read_json()
            .with_context(|| format!("unexpected response from {service}")),
        401 => bail!("{service} rejected the API key (HTTP 401)"),
        404 => bail!("{service}: not found (HTTP 404)"),
        429 => bail!("{service} is rate limiting requests (HTTP 429), try again later"),
        status => bail!("{service} request failed (HTTP {status})"),
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

pub(crate) fn normalise(title: &str) -> String {
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

/// Extra distance for a candidate whose running time disagrees with the file's. Small
/// differences are expected from PAL speed-up, credits and rounding to the minute.
pub fn length_penalty(length: Option<Length>, candidate: &Candidate) -> f64 {
    let (Some(length), Some(minutes)) = (length, candidate.runtime) else {
        return 0.0;
    };
    let expected = f64::from(minutes) * 60.0;
    let penalty = match (length.seconds - expected).abs() / expected {
        ratio if ratio <= 0.06 => 0.0,
        ratio if ratio <= 0.12 => 0.03,
        _ => 0.15,
    };
    if length.alternate_cut {
        penalty / 3.0
    } else {
        penalty
    }
}

/// How far a file's running time may stray from its episodes' before it can't be them, as a
/// ratio either way. Sources round episode runtimes and often give nominal ones (a 22-minute
/// episode listed as 30), so only a gross difference counts: a short in a double episode's
/// place, or two episodes in a file numbered as one.
const EPISODE_LENGTH_RATIO: f64 = 1.5;

/// The episodes' combined runtime in minutes, when the source gives every one.
pub fn episodes_runtime(episodes: &[&EpisodeInfo]) -> Option<u32> {
    episodes.iter().map(|episode| episode.runtime).sum()
}

/// Whether a file running `seconds` can hold `episodes`; true when either length is unknown.
pub fn episodes_fit(seconds: Option<f64>, episodes: &[&EpisodeInfo]) -> bool {
    let (Some(seconds), Some(minutes)) = (seconds, episodes_runtime(episodes)) else {
        return true;
    };
    let ratio = seconds / (f64::from(minutes) * 60.0);
    (EPISODE_LENGTH_RATIO.recip()..=EPISODE_LENGTH_RATIO).contains(&ratio)
}

/// Sorts candidates by distance, preserving the source's own ranking among ties.
pub fn rank(
    title: &str,
    year: Option<i32>,
    length: Option<Length>,
    candidates: Vec<Candidate>,
) -> Vec<(f64, Candidate)> {
    let mut ranked: Vec<_> = candidates
        .into_iter()
        .map(|candidate| {
            let score = distance(title, year, &candidate) + length_penalty(length, &candidate);
            (score.min(1.0), candidate)
        })
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

    pub struct Reply {
        pub status: u16,
        pub headers: Vec<(&'static str, String)>,
        pub body: String,
    }

    impl From<(u16, String)> for Reply {
        fn from((status, body): (u16, String)) -> Self {
            Reply {
                status,
                headers: Vec::new(),
                body,
            }
        }
    }

    type Handler = dyn Fn(&Request) -> Reply + Send + Sync;

    pub struct Server {
        pub base: String,
        requests: Arc<Mutex<Vec<Request>>>,
    }

    impl Server {
        /// Starts a server answering every request with the reply `handler` returns, usually a
        /// status and JSON body.
        pub fn start<R: Into<Reply>>(
            handler: impl Fn(&Request) -> R + Send + Sync + 'static,
        ) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let handler: Arc<Handler> = Arc::new(move |request: &Request| handler(request).into());
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
        handler: &Handler,
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
        let reply = handler(&request);
        log.lock().unwrap().push(request);

        let mut stream = stream;
        write!(stream, "HTTP/1.1 {} Status\r\n", reply.status)?;
        for (name, value) in &reply.headers {
            write!(stream, "{name}: {value}\r\n")?;
        }
        write!(
            stream,
            "Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            reply.body.len(),
            reply.body
        )?;
        stream.flush()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::testserver::{Reply, Server};
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
            runtime: None,
            episodes: None,
        }
    }

    #[test]
    fn running_times_separate_films_of_the_same_name() {
        let mut short = candidate("Heat", 1995);
        short.id = "short".into();
        short.runtime = Some(90);
        let mut long = candidate("Heat", 1995);
        long.id = "long".into();
        long.runtime = Some(170);
        let length = |seconds, alternate_cut| {
            Some(Length {
                seconds,
                alternate_cut,
            })
        };

        let tied = rank("Heat", Some(1995), None, vec![short.clone(), long.clone()]);
        assert!(!is_strong(&tied, 0.08));

        let ranked = rank(
            "Heat",
            Some(1995),
            length(170.0 * 60.0, false),
            vec![short.clone(), long.clone()],
        );
        assert_eq!(ranked[0].1.id, "long");
        assert!(is_strong(&ranked, 0.08));

        // PAL speed-up runs about 4% short and is not penalised.
        assert_eq!(
            length_penalty(length(170.0 * 60.0 * 0.96, false), &long),
            0.0
        );
        assert!(length_penalty(length(195.0 * 60.0, false), &long) > 0.0);
        assert!(
            length_penalty(length(200.0 * 60.0, true), &long)
                < length_penalty(length(200.0 * 60.0, false), &long)
        );
        assert_eq!(length_penalty(None, &long), 0.0);
        assert_eq!(
            length_penalty(length(60.0, false), &candidate("Heat", 1995)),
            0.0
        );
    }

    #[test]
    fn exact_match_is_strong() {
        let ranked = rank(
            "the matrix",
            Some(1999),
            None,
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
            None,
            vec![candidate("Dune", 2021), candidate("Dune", 1984)],
        );
        assert!(!is_strong(&ranked, 0.08));
        let ranked = rank(
            "Dune",
            Some(1984),
            None,
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
        let ranked = rank("Heat", Some(1995), None, vec![first, second]);
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

    fn fast_http() -> Http {
        let mut http = Http::new("TMDB");
        http.retry.backoff = Duration::ZERO;
        http.retry.max_wait = Duration::ZERO;
        http
    }

    fn fetch(http: &Http, server: &Server) -> Result<serde_json::Value> {
        http.json(|agent| agent.get(&server.base).call())
    }

    /// A server replying with each of `statuses` in turn, then with `{"ok": true}`.
    fn sequence(statuses: &[u16]) -> Server {
        let statuses = statuses.to_vec();
        let count = AtomicUsize::new(0);
        Server::start(move |_| {
            let status = statuses.get(count.fetch_add(1, Ordering::SeqCst));
            (*status.unwrap_or(&200), r#"{"ok": true}"#.to_string())
        })
    }

    fn response(status: u16, retry_after: Option<&str>) -> Response {
        let mut builder = ureq::http::Response::builder().status(status);
        if let Some(value) = retry_after {
            builder = builder.header("Retry-After", value);
        }
        Ok(builder.body(ureq::Body::builder().data("")).unwrap())
    }

    #[test]
    fn http_failures_become_helpful_errors() {
        let failure = |status| {
            let server = Server::start(move |_| (status, "{}".to_string()));
            fetch(&fast_http(), &server).unwrap_err().to_string()
        };
        assert_eq!(failure(401), "TMDB rejected the API key (HTTP 401)");
        assert_eq!(failure(404), "TMDB: not found (HTTP 404)");
        assert_eq!(
            failure(429),
            "TMDB is rate limiting requests (HTTP 429), try again later"
        );
        assert_eq!(failure(500), "TMDB request failed (HTTP 500)");
    }

    #[test]
    fn transient_failures_are_retried() {
        let server = sequence(&[429, 503, 502, 504]);
        let value = fetch(&fast_http(), &server).unwrap();
        assert_eq!(value["ok"], true);
        assert_eq!(server.requests().len(), 5);
    }

    #[test]
    fn retries_give_up_after_the_last_attempt() {
        let server = sequence(&[429; 10]);
        let mut http = fast_http();
        http.retry.attempts = 3;
        assert!(fetch(&http, &server).is_err());
        assert_eq!(server.requests().len(), 3);
    }

    #[test]
    fn permanent_failures_are_not_retried() {
        for status in [400, 401, 404, 500] {
            let server = sequence(&[status]);
            assert!(fetch(&fast_http(), &server).is_err());
            assert_eq!(server.requests().len(), 1, "HTTP {status}");
        }
    }

    #[test]
    fn retry_after_is_honoured() {
        let server = Server::start({
            let count = AtomicUsize::new(0);
            move |_| -> Reply {
                if count.fetch_add(1, Ordering::SeqCst) == 0 {
                    Reply {
                        status: 429,
                        headers: vec![("Retry-After", "0".into())],
                        body: "{}".into(),
                    }
                } else {
                    (200, "[]".to_string()).into()
                }
            }
        });
        let mut http = Http::new("TMDB");
        http.retry.backoff = Duration::from_secs(3600);
        assert_eq!(fetch(&http, &server).unwrap(), serde_json::json!([]));
    }

    #[test]
    fn waits_back_off_and_are_capped() {
        let retry = Retry {
            attempts: 5,
            backoff: Duration::from_secs(1),
            max_wait: Duration::from_secs(10),
        };
        let wait = |n, result: &Response| retry.wait(n, result).map(|(wait, _)| wait.as_secs());
        let busy = response(503, None);
        assert_eq!(wait(1, &busy), Some(1));
        assert_eq!(wait(2, &busy), Some(2));
        assert_eq!(wait(3, &busy), Some(4));
        assert_eq!(wait(5, &busy), Some(10));
        assert_eq!(wait(1, &response(429, Some("7"))), Some(7));
        assert_eq!(wait(1, &response(429, Some("120"))), Some(10));
        assert_eq!(
            wait(2, &response(429, Some("Wed, 21 Oct 2015 07:28:00 GMT"))),
            Some(2),
            "HTTP dates fall back to the backoff"
        );
        assert_eq!(wait(1, &Err(ureq::Error::ConnectionFailed)), Some(1));
        assert_eq!(wait(1, &Err(ureq::Error::HostNotFound)), None);
        assert_eq!(wait(1, &response(500, None)), None);
        assert_eq!(wait(1, &response(200, None)), None);
    }

    #[test]
    fn only_grossly_different_lengths_rule_out_an_episode() {
        let runs = |minutes| EpisodeInfo {
            season: 5,
            number: 1,
            title: None,
            air_date: None,
            runtime: Some(minutes),
        };
        let (short, special) = (runs(10), runs(45));
        assert!(
            episodes_fit(Some(670.0), &[&short]),
            "rounded runtimes pass"
        );
        assert!(episodes_fit(Some(1354.0), &[&runs(25)]));
        assert!(
            episodes_fit(Some(1320.0), &[&runs(30)]),
            "nominal runtimes pass"
        );
        assert!(
            !episodes_fit(Some(670.0), &[&special]),
            "a short is not a special"
        );
        assert!(
            !episodes_fit(Some(1354.0), &[&short]),
            "a double is not a single"
        );
        assert!(episodes_fit(
            Some(2710.0),
            &[&runs(11), &runs(11), &runs(11), &runs(11)]
        ));
        assert!(episodes_fit(None, &[&special]));
        let unknown = EpisodeInfo {
            runtime: None,
            ..short.clone()
        };
        assert!(
            episodes_fit(Some(670.0), &[&special, &unknown]),
            "a partial total proves nothing"
        );
    }

    #[test]
    fn episode_counts_list_specials_separately() {
        let episode = |season| EpisodeInfo {
            season,
            number: 1,
            title: None,
            air_date: None,
            runtime: None,
        };
        let count = |seasons: &[u32]| {
            let list: Vec<_> = seasons.iter().map(|&season| episode(season)).collect();
            EpisodeCount::of(&list).to_string()
        };
        assert_eq!(count(&[1, 1, 1, 1, 1, 0, 0]), "5 episodes, 2 specials");
        assert_eq!(count(&[1, 0]), "1 episode, 1 special");
        assert_eq!(count(&[1, 2]), "2 episodes");
        assert_eq!(count(&[]), "0 episodes");
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
        assert_eq!(show_source_named(&config, "tvdb").unwrap().name(), "tvdb");
        assert!(show_source_named(&config, "imdb").is_err());
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
