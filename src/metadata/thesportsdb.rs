//! TheSportsDB (API v1). Leagues stand in for shows, their seasons for seasons, and events for
//! date-based episodes.

use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::{Candidate, EpisodeInfo, Http, ShowSource, non_empty, normalise};
use crate::config::MetadataConfig;

const BASE: &str = "https://www.thesportsdb.com/api/v1/json";

/// The key TheSportsDB gives everyone, which caps how many results each request returns.
const FREE_KEY: &str = "123";

/// How many leagues a search offers.
const SEARCH_RESULTS: usize = 10;

pub struct TheSportsDb {
    base: String,
    http: Http,
    key: String,
    /// Every league TheSportsDB knows, fetched for the first search, since v1 can't search them.
    leagues: OnceCell<Vec<LeagueSummary>>,
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

#[derive(Deserialize)]
struct Event {
    #[serde(rename = "idEvent")]
    id: String,
    #[serde(rename = "strEvent")]
    name: Option<String>,
    #[serde(rename = "dateEvent")]
    date: Option<String>,
    #[serde(rename = "strTimestamp")]
    timestamp: Option<String>,
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
                    "note: using TheSportsDB's free API key, which returns only the first few \
                     leagues and events of each request; set metadata.thesportsdb_api_key or \
                     THESPORTSDB_API_KEY for complete results"
                );
                FREE_KEY.to_string()
            }
            key => key.to_string(),
        };
        TheSportsDb {
            base: base.to_string(),
            http: Http::new("TheSportsDB"),
            key,
            leagues: OnceCell::new(),
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

    fn events(&self, id: &str, season: &str) -> Result<Vec<Event>> {
        let found: Events = self.get("eventsseason.php", &[("id", id), ("s", season)])?;
        Ok(found.events.unwrap_or_default())
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

    fn show(&self, id: &str) -> Result<Candidate> {
        let id = entered_id(id);
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
        client.http.retry.backoff = std::time::Duration::ZERO;
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
