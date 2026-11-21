//! TheSportsDB (API v1). Leagues stand in for shows, their seasons for seasons, and events for
//! date-based episodes.

use std::cell::{OnceCell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::{Candidate, EpisodeInfo, Http, LinkedEpisode, ShowSource, non_empty, normalise};
use crate::config::MetadataConfig;

const BASE: &str = "https://www.thesportsdb.com/api/v1/json";

/// The key TheSportsDB gives everyone, which caps how many results each request returns.
const FREE_KEY: &str = "123";

/// The most events the free key lists of a season; a round's events are listed in full.
const FREE_SEASON_EVENTS: usize = 15;

/// Round numbers from here on stand for stages rather than rounds: 500 for pre-season, 125 to
/// 200 for playoffs.
const SPECIAL_ROUNDS: u32 = 100;

/// Rounds in a row found empty before a season is taken to be over, so that a cancelled round
/// doesn't end it early.
const EMPTY_ROUNDS: u32 = 3;

/// How many leagues a search offers.
const SEARCH_RESULTS: usize = 10;

pub struct TheSportsDb {
    base: String,
    http: Http,
    key: String,
    /// Every league TheSportsDB knows, fetched for the first search, since v1 can't search them.
    leagues: OnceCell<Vec<LeagueSummary>>,
    /// Each season's events by league and season name, as the free key takes a request per round.
    seasons: RefCell<HashMap<(String, String), Vec<Event>>>,
}

#[derive(Deserialize)]
struct Leagues<T> {
    leagues: Option<Vec<T>>,
}

#[derive(Deserialize)]
struct LeagueSummary {
    #[serde(rename = "idLeague")]
    id: String,
    #[serde(rename = "strLeague")]
    name: String,
    #[serde(rename = "strSport")]
    sport: Option<String>,
}

#[derive(Deserialize)]
struct League {
    #[serde(rename = "idLeague")]
    id: String,
    #[serde(rename = "strLeague")]
    name: String,
    #[serde(rename = "strLeagueAlternate")]
    alternate: Option<String>,
    #[serde(rename = "strDescriptionEN")]
    description: Option<String>,
}

#[derive(Deserialize)]
struct Seasons {
    seasons: Option<Vec<Season>>,
}

#[derive(Deserialize)]
struct Season {
    #[serde(rename = "strSeason")]
    name: String,
}

#[derive(Deserialize)]
struct Events {
    events: Option<Vec<Event>>,
}

#[derive(Clone, Deserialize)]
struct Event {
    #[serde(rename = "idEvent")]
    id: String,
    #[serde(rename = "strEvent")]
    name: Option<String>,
    #[serde(rename = "dateEvent")]
    date: Option<String>,
    #[serde(rename = "strTimestamp")]
    timestamp: Option<String>,
    #[serde(rename = "idLeague")]
    league_id: Option<String>,
    #[serde(rename = "intRound")]
    round: Option<String>,
    #[serde(rename = "strLeague")]
    league: Option<String>,
}

/// The event ID in an event page URL such as
/// `https://www.thesportsdb.com/event/2408215-las-vegas-grand-prix-qualifying`.
fn event_id(input: &str) -> Option<&str> {
    let (_, rest) = input.trim().split_once("/event/")?;
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    Some(&rest[..end]).filter(|id| !id.is_empty())
}

fn league_url(id: &str) -> String {
    format!("https://www.thesportsdb.com/league/{id}")
}

/// Accepts a bare league ID or a league page URL such as
/// `https://www.thesportsdb.com/league/4370-formula-1`.
fn entered_id(input: &str) -> &str {
    let input = input.trim();
    let segment = match input.split_once("/league/") {
        Some((_, rest)) => rest,
        None => input,
    };
    let end = segment
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(segment.len());
    match &segment[..end] {
        "" => input,
        id => id,
    }
}

/// The calendar years a season such as `2024` or `2023-2024` spans.
fn season_years(name: &str) -> Option<(u32, u32)> {
    let mut years = name
        .split(['-', '/'])
        .map(|year| year.trim().parse::<u32>());
    let first = years.next()?.ok()?;
    let last = match years.next() {
        Some(Ok(last)) if last >= first => last,
        // Short forms such as `2023-24`, or `1999-00` across a century.
        Some(Ok(last)) if last < 100 => {
            let last = first / 100 * 100 + last;
            if last < first { last + 100 } else { last }
        }
        _ => first,
    };
    Some((first, last))
}

/// Season names to try for seasons starting in `year` when the league's season list doesn't have
/// them, as it won't with the free key, which lists only the first few.
fn guessed_seasons(year: u32) -> [String; 2] {
    [year.to_string(), format!("{year}-{}", year + 1)]
}

/// Seasons grouped by the year they start in, which numbers them.
type SeasonsByStart = BTreeMap<u32, BTreeSet<String>>;

fn by_start<'a>(names: impl IntoIterator<Item = &'a String>) -> SeasonsByStart {
    let mut seasons = SeasonsByStart::new();
    for name in names {
        if let Some((first, _)) = season_years(name) {
            seasons.entry(first).or_default().insert(name.clone());
        }
    }
    seasons
}

impl TheSportsDb {
    pub fn new(config: &MetadataConfig) -> Self {
        Self::with_base(config, BASE)
    }

    fn with_base(config: &MetadataConfig, base: &str) -> Self {
        let key = match config.thesportsdb_api_key.as_str() {
            "" => {
                eprintln!(
                    "note: using TheSportsDB's free API key, which finds few leagues by name and \
                     is slow to list a season's events; set metadata.thesportsdb_api_key or \
                     THESPORTSDB_API_KEY to lift its limits"
                );
                FREE_KEY.to_string()
            }
            key => key.to_string(),
        };
        let mut http = Http::new("TheSportsDB");
        // The free key's rate limit asks for waits of two minutes, which retrying sooner only
        // extends.
        http.retry.max_wait = Duration::from_secs(150);
        TheSportsDb {
            base: base.to_string(),
            http,
            key,
            leagues: OnceCell::new(),
            seasons: RefCell::default(),
        }
    }

    fn get<T: DeserializeOwned>(&self, endpoint: &str, query: &[(&str, &str)]) -> Result<T> {
        self.http.json(|agent| {
            let mut request = agent.get(format!("{}/{}/{endpoint}", self.base, self.key));
            for (name, value) in query {
                request = request.query(*name, *value);
            }
            request.call()
        })
    }

    fn leagues(&self) -> Result<&[LeagueSummary]> {
        if let Some(leagues) = self.leagues.get() {
            return Ok(leagues);
        }
        let found: Leagues<LeagueSummary> = self.get("all_leagues.php", &[])?;
        Ok(self
            .leagues
            .get_or_init(|| found.leagues.unwrap_or_default()))
    }

    fn season_names(&self, id: &str) -> Result<Vec<String>> {
        let found: Seasons = self.get("search_all_seasons.php", &[("id", id)])?;
        Ok(found
            .seasons
            .unwrap_or_default()
            .into_iter()
            .map(|season| season.name)
            .collect())
    }

    /// A season's events. Where the free key cuts the season's list short, its rounds are listed
    /// one by one; events outside any round beyond the cut are still missed.
    fn events(&self, id: &str, season: &str) -> Result<Vec<Event>> {
        let key = (id.to_string(), season.to_string());
        if let Some(events) = self.seasons.borrow().get(&key) {
            return Ok(events.clone());
        }
        let found: Events = self.get("eventsseason.php", &[("id", id), ("s", season)])?;
        let mut events = found.events.unwrap_or_default();
        if self.key == FREE_KEY && events.len() >= FREE_SEASON_EVENTS {
            // The list's last round may be cut short, but those before it are whole.
            let last = events
                .iter()
                .filter_map(|event| event.round.as_deref()?.parse().ok())
                .filter(|&round| round < SPECIAL_ROUNDS)
                .max()
                .unwrap_or(1);
            events.extend(self.events_by_round(id, season, last)?);
        }
        self.seasons.borrow_mut().insert(key, events.clone());
        Ok(events)
    }

    /// The events of each round from `first` on.
    fn events_by_round(&self, id: &str, season: &str, first: u32) -> Result<Vec<Event>> {
        let mut events = Vec::new();
        let mut empty = 0;
        for round in first.max(1).. {
            let round = round.to_string();
            let found: Events = self.get(
                "eventsround.php",
                &[("id", id), ("r", &round), ("s", season)],
            )?;
            match found.events {
                Some(listed) if !listed.is_empty() => {
                    empty = 0;
                    events.extend(listed);
                }
                _ => {
                    empty += 1;
                    if empty == EMPTY_ROUNDS {
                        break;
                    }
                }
            }
        }
        Ok(events)
    }

    /// The event with ID `id`, and the ID of its league.
    fn event(&self, id: &str) -> Result<(Event, String)> {
        let found: Events = self.get("lookupevent.php", &[("id", id)])?;
        let Some(mut event) = found.events.unwrap_or_default().into_iter().next() else {
            anyhow::bail!("TheSportsDB: no event {id}");
        };
        let Some(league) = non_empty(event.league_id.take()) else {
            anyhow::bail!("TheSportsDB: event {id} belongs to no league");
        };
        Ok((event, league))
    }

    /// The events of `seasons`, numbered within each starting year in the order they take place.
    fn fetch_seasons(&self, id: &str, seasons: SeasonsByStart) -> Result<Vec<EpisodeInfo>> {
        let mut episodes = Vec::new();
        for (year, names) in seasons {
            let mut events = Vec::new();
            for name in names {
                events.extend(self.events(id, &name)?);
            }
            episodes.extend(numbered(year, events));
        }
        Ok(episodes)
    }
}

/// Numbers a season's events in the order they take place, as episodes of season `year`.
fn numbered(year: u32, mut events: Vec<Event>) -> Vec<EpisodeInfo> {
    let id_number = |event: &Event| event.id.parse::<u64>().unwrap_or(u64::MAX);
    events.sort_by(|a, b| {
        (&a.date, &a.timestamp, id_number(a)).cmp(&(&b.date, &b.timestamp, id_number(b)))
    });
    events.dedup_by(|a, b| a.id == b.id);
    events
        .into_iter()
        .zip(1..)
        .map(|(event, number)| EpisodeInfo {
            season: year,
            number,
            title: non_empty(event.name),
            air_date: non_empty(event.date),
            runtime: None,
        })
        .collect()
}

impl ShowSource for TheSportsDb {
    fn name(&self) -> &'static str {
        "thesportsdb"
    }

    fn search_shows(&self, title: &str, _year: Option<i32>) -> Result<Vec<Candidate>> {
        let wanted = normalise(title);
        let mut scored: Vec<(f64, &LeagueSummary)> = self
            .leagues()?
            .iter()
            .filter_map(|league| {
                let name = normalise(&league.name);
                let similarity = strsim::normalized_levenshtein(&wanted, &name);
                let contains =
                    !wanted.is_empty() && (name.contains(&wanted) || wanted.contains(&name));
                (similarity >= 0.5 || contains).then_some((similarity, league))
            })
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        Ok(scored
            .into_iter()
            .take(SEARCH_RESULTS)
            .map(|(_, league)| Candidate {
                source: "thesportsdb",
                id: league.id.clone(),
                title: league.name.clone(),
                year: None,
                url: league_url(&league.id),
                original_title: None,
                overview: league.sport.clone(),
                runtime: None,
                episodes: None,
            })
            .collect())
    }

    /// Takes a league's ID or page URL, or an event's page URL for the league it is part of.
    fn show(&self, id: &str) -> Result<Candidate> {
        let league;
        let id = match event_id(id) {
            Some(event) => {
                league = self.event(event)?.1;
                &league
            }
            None => entered_id(id),
        };
        anyhow::ensure!(
            !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()),
            "TheSportsDB: {id:?} is not a league ID, nor a league or event page URL"
        );
        let found: Leagues<League> = self.get("lookupleague.php", &[("id", id)])?;
        let Some(league) = found.leagues.unwrap_or_default().into_iter().next() else {
            anyhow::bail!("TheSportsDB: no league {id}");
        };
        Ok(Candidate {
            source: "thesportsdb",
            url: league_url(&league.id),
            id: league.id,
            original_title: non_empty(league.alternate),
            overview: non_empty(league.description),
            title: league.name,
            year: None,
            runtime: None,
            episodes: None,
        })
    }

    fn episodes(&self, id: &str, season: Option<u32>) -> Result<Vec<EpisodeInfo>> {
        let mut seasons = by_start(&self.season_names(id)?);
        if let Some(year) = season {
            seasons.retain(|&first, _| first == year);
            if seasons.is_empty() {
                seasons.insert(year, guessed_seasons(year).into());
            }
        }
        self.fetch_seasons(id, seasons)
    }

    /// Only the seasons the dates fall in: fetching every season since a league began would take
    /// one request per season.
    fn episodes_aired(&self, id: &str, dates: &BTreeSet<String>) -> Result<Vec<EpisodeInfo>> {
        let known = self.season_names(id)?;
        let years: BTreeSet<u32> = dates
            .iter()
            .filter_map(|date| date.get(..4)?.parse().ok())
            .collect();
        let mut wanted = SeasonsByStart::new();
        for year in years {
            let covering = by_start(known.iter().filter(|name| {
                season_years(name).is_some_and(|(first, last)| (first..=last).contains(&year))
            }));
            if covering.is_empty() {
                let [single, split] = guessed_seasons(year);
                wanted.entry(year).or_default().extend([single, split]);
                let before = year.saturating_sub(1);
                wanted
                    .entry(before)
                    .or_default()
                    .insert(format!("{before}-{year}"));
            }
            for (first, names) in covering {
                wanted.entry(first).or_default().extend(names);
            }
        }
        self.fetch_seasons(id, wanted)
    }

    fn linked_episode(&self, input: &str) -> Result<Option<LinkedEpisode>> {
        let Some(id) = event_id(input) else {
            return Ok(None);
        };
        let (event, show_id) = self.event(id)?;
        Ok(Some(LinkedEpisode {
            show_id,
            show: non_empty(event.league),
            title: non_empty(event.name),
            air_date: non_empty(event.date),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::testserver::{Request, Server};

    fn client(server: &Server, key: &str) -> TheSportsDb {
        let config = MetadataConfig {
            thesportsdb_api_key: key.into(),
            ..MetadataConfig::default()
        };
        let mut client = TheSportsDb::with_base(&config, &server.base);
        client.http.retry.backoff = Duration::ZERO;
        client
    }

    fn ok(body: &str) -> (u16, String) {
        (200, body.to_string())
    }

    const LEAGUES: &str = r#"{"leagues": [
        {"idLeague": "4328", "strLeague": "English Premier League", "strSport": "Soccer"},
        {"idLeague": "4370", "strLeague": "Formula 1", "strSport": "Motorsport"},
        {"idLeague": "4371", "strLeague": "Formula 2", "strSport": "Motorsport"},
        {"idLeague": "4391", "strLeague": "NFL", "strSport": "American Football"}
    ]}"#;

    fn event(id: &str, name: &str, date: &str, time: &str) -> String {
        format!(
            r#"{{"idEvent": "{id}", "strEvent": "{name}", "dateEvent": "{date}",
                 "strTimestamp": "{date}T{time}"}}"#
        )
    }

    fn league_server() -> Server {
        Server::start(|request: &Request| {
            let path = request
                .path()
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .to_string();
            let season = request.param("s").unwrap_or_default();
            match path.as_str() {
                "all_leagues.php" => ok(LEAGUES),
                "lookupleague.php" if request.param("id").as_deref() == Some("4370") => ok(
                    r#"{"leagues": [{"idLeague": "4370", "strLeague": "Formula 1",
                        "strLeagueAlternate": "F1, Formula One",
                        "strDescriptionEN": "The highest class of single-seater racing."}]}"#,
                ),
                "lookupleague.php" => ok(r#"{"leagues": null}"#),
                "lookupevent.php" if request.param("id").as_deref() == Some("2408215") => {
                    ok(r#"{"events": [{"idEvent": "2408215", "idLeague": "4370",
                        "strLeague": "Formula 1", "strEvent": "Las Vegas Grand Prix Qualifying",
                        "dateEvent": "2026-11-21", "strTimestamp": "2026-11-21T04:00:00"}]}"#)
                }
                "lookupevent.php" => ok(r#"{"events": null}"#),
                "search_all_seasons.php" => ok(
                    r#"{"seasons": [{"strSeason": "2022-2023"}, {"strSeason": "2023-2024"},
                        {"strSeason": "2024-2025"}]}"#,
                ),
                "eventsseason.php" if season == "2023-2024" => ok(&format!(
                    r#"{{"events": [{}, {}, {}]}}"#,
                    event("3", "Chelsea vs Arsenal", "2024-03-02", "17:30:00"),
                    event("1", "Arsenal vs Spurs", "2023-08-12", "15:00:00"),
                    event("2", "Spurs vs Arsenal", "2024-03-02", "15:00:00"),
                )),
                "eventsseason.php" => ok(r#"{"events": null}"#),
                _ => (404, "{}".into()),
            }
        })
    }

    #[test]
    fn the_free_key_is_used_when_none_is_configured() {
        let server = league_server();
        client(&server, "").search_shows("Formula 1", None).unwrap();
        client(&server, "premium")
            .search_shows("Formula 1", None)
            .unwrap();
        let paths = server.paths();
        assert_eq!(paths[0], "/123/all_leagues.php");
        assert_eq!(paths[1], "/premium/all_leagues.php");
    }

    #[test]
    fn searches_rank_leagues_by_name_and_fetch_the_list_once() {
        let server = league_server();
        let client = client(&server, "key");
        let found = client.search_shows("Formula 1", None).unwrap();
        let titles: Vec<_> = found.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(titles, ["Formula 1", "Formula 2"]);
        assert_eq!(found[0].id, "4370");
        assert_eq!(found[0].url, "https://www.thesportsdb.com/league/4370");
        assert_eq!(found[0].overview.as_deref(), Some("Motorsport"));
        assert_eq!(found[0].year, None);

        let found = client.search_shows("Premier League", None).unwrap();
        assert_eq!(
            found[0].title, "English Premier League",
            "a name containing the query"
        );
        assert!(client.search_shows("Curling", None).unwrap().is_empty());
        assert_eq!(server.paths().len(), 1);
    }

    #[test]
    fn events_are_looked_up_by_page_url() {
        let server = league_server();
        let client = client(&server, "key");
        let linked = client
            .linked_episode(
                "https://www.thesportsdb.com/event/2408215-las-vegas-grand-prix-qualifying",
            )
            .unwrap();
        assert_eq!(
            linked,
            Some(LinkedEpisode {
                show_id: "4370".into(),
                show: Some("Formula 1".into()),
                title: Some("Las Vegas Grand Prix Qualifying".into()),
                air_date: Some("2026-11-21".into()),
            })
        );
        assert_eq!(client.linked_episode("Las Vegas").unwrap(), None);
        let league = client
            .show("https://www.thesportsdb.com/event/2408215-las-vegas-grand-prix-qualifying")
            .unwrap();
        assert_eq!(
            (league.id.as_str(), league.title.as_str()),
            ("4370", "Formula 1")
        );
        assert!(
            client
                .show("Formula 1")
                .unwrap_err()
                .to_string()
                .contains("not a league ID")
        );
        assert_eq!(client.linked_episode("2026").unwrap(), None);
        assert!(
            client
                .linked_episode("https://www.thesportsdb.com/event/1")
                .unwrap_err()
                .to_string()
                .contains("no event 1")
        );
    }

    #[test]
    fn a_season_cut_short_by_the_free_key_is_listed_round_by_round() {
        // Rounds 1–8 of three events each, with round 6 cancelled; the season's list stops at
        // the free key's first fifteen, the end of round 5.
        let all: Vec<(u32, String)> = [1, 2, 3, 4, 5, 7, 8]
            .into_iter()
            .flat_map(|round| {
                (1..=3).map(move |session| {
                    let date = format!("2026-{round:02}-0{session}");
                    let id = (round * 10 + session).to_string();
                    let event = event(&id, &format!("Round {round} {session}"), &date, "12:00");
                    // Pre-season testing, numbered as TheSportsDB does.
                    let listed = if (round, session) == (1, 1) {
                        500
                    } else {
                        round
                    };
                    let field = format!(r#"{{"intRound": "{listed}", "#);
                    (round, event.replacen('{', &field, 1))
                })
            })
            .collect();
        let listing = |events: Vec<&String>| {
            let events: Vec<&str> = events.into_iter().map(String::as_str).collect();
            ok(&format!(r#"{{"events": [{}]}}"#, events.join(",")))
        };
        let server = Server::start(move |request: &Request| {
            let path = request.path();
            if path.ends_with("search_all_seasons.php") {
                ok(r#"{"seasons": [{"strSeason": "2026"}]}"#)
            } else if path.ends_with("eventsseason.php") {
                listing(
                    all.iter()
                        .take(FREE_SEASON_EVENTS)
                        .map(|(_, e)| e)
                        .collect(),
                )
            } else {
                let round: u32 = request.param("r").unwrap().parse().unwrap();
                match all
                    .iter()
                    .filter(|(r, _)| *r == round)
                    .map(|(_, e)| e)
                    .collect::<Vec<_>>()
                {
                    found if found.is_empty() => ok(r#"{"events": null}"#),
                    found => listing(found),
                }
            }
        });
        let free = client(&server, "");
        let episodes = free.episodes("4370", Some(2026)).unwrap();
        assert_eq!(episodes.len(), 21, "every event, once");
        assert_eq!(episodes.last().unwrap().title.as_deref(), Some("Round 8 3"));
        assert_eq!(episodes.last().unwrap().number, 21);

        free.episodes("4370", Some(2026)).unwrap();
        let rounds: Vec<_> = server
            .requests()
            .iter()
            .filter_map(|r| r.param("r"))
            .collect();
        assert_eq!(
            rounds,
            ["5", "6", "7", "8", "9", "10", "11"],
            "rounds the list covers aren't fetched again, a cancelled round doesn't end the \
             season, and seasons are fetched once"
        );

        let paid = league_server();
        client(&paid, "key").episodes("4328", Some(2023)).unwrap();
        assert!(paid.paths().iter().all(|p| !p.ends_with("eventsround.php")));
    }

    #[test]
    fn leagues_are_looked_up_by_id_or_page_url() {
        let server = league_server();
        let client = client(&server, "key");
        let league = client
            .show("https://www.thesportsdb.com/league/4370-formula-1")
            .unwrap();
        assert_eq!(league.title, "Formula 1");
        assert_eq!(league.original_title.as_deref(), Some("F1, Formula One"));
        assert!(league.overview.unwrap().starts_with("The highest class"));
        assert!(
            client
                .show("1")
                .unwrap_err()
                .to_string()
                .contains("no league 1")
        );
    }

    #[test]
    fn entered_ids_accept_urls() {
        assert_eq!(entered_id(" 4370 "), "4370");
        assert_eq!(
            entered_id("https://www.thesportsdb.com/league/4370-formula-1"),
            "4370"
        );
        assert_eq!(entered_id("thesportsdb.com/league/4328"), "4328");
        assert_eq!(entered_id("nonsense"), "nonsense");
    }

    #[test]
    fn season_names_give_the_years_they_span() {
        assert_eq!(season_years("2024"), Some((2024, 2024)));
        assert_eq!(season_years("2023-2024"), Some((2023, 2024)));
        assert_eq!(season_years("2023-24"), Some((2023, 2024)));
        assert_eq!(season_years("1999-00"), Some((1999, 2000)));
        assert_eq!(season_years("Season"), None);
    }

    #[test]
    fn events_are_numbered_in_the_order_they_take_place() {
        let server = league_server();
        let episodes = client(&server, "key").episodes("4328", Some(2023)).unwrap();
        let listed: Vec<_> = episodes
            .iter()
            .map(|e| {
                (
                    e.season,
                    e.number,
                    e.title.as_deref().unwrap(),
                    e.air_date.as_deref().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            listed,
            [
                (2023, 1, "Arsenal vs Spurs", "2023-08-12"),
                (2023, 2, "Spurs vs Arsenal", "2024-03-02"),
                (2023, 3, "Chelsea vs Arsenal", "2024-03-02"),
            ]
        );
    }

    #[test]
    fn dates_fetch_only_the_seasons_they_fall_in() {
        let server = league_server();
        let dates = BTreeSet::from(["2024-03-02".to_string()]);
        let episodes = client(&server, "key")
            .episodes_aired("4328", &dates)
            .unwrap();
        assert_eq!(episodes.len(), 3);
        let seasons: Vec<_> = server
            .requests()
            .iter()
            .filter(|r| r.path().ends_with("eventsseason.php"))
            .map(|r| r.param("s").unwrap())
            .collect();
        assert_eq!(seasons, ["2023-2024", "2024-2025"]);
    }

    #[test]
    fn unlisted_seasons_are_guessed() {
        let server = Server::start(|request: &Request| match request.path() {
            path if path.ends_with("search_all_seasons.php") => {
                ok(r#"{"seasons": [{"strSeason": "1950"}]}"#)
            }
            _ => ok(r#"{"events": null}"#),
        });
        let dates = BTreeSet::from(["2024-03-02".to_string()]);
        client(&server, "").episodes_aired("4370", &dates).unwrap();
        let seasons: Vec<_> = server
            .requests()
            .iter()
            .filter_map(|r| r.param("s"))
            .collect();
        assert_eq!(seasons, ["2023-2024", "2024", "2024-2025"]);
    }
}
