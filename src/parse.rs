//! Guesses what a video file is from its name and parent directories.

use std::path::Path;
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;

use crate::model::Kind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub kind: Kind,
    pub title: String,
    pub year: Option<i32>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub episode_end: Option<u32>,
    pub air_date: Option<String>,
    pub episode_title: Option<String>,
    pub edition: Option<String>,
    pub part: Option<u32>,
    pub resolution: Option<String>,
    /// `(source, id)` from a Plex-style `{tmdb-123}` tag.
    pub id_hint: Option<(String, String)>,
}

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid regex")
}

static TAG: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\{([a-z]+)-([^}]+)\}"));
static LEADING_GROUP: LazyLock<Regex> = LazyLock::new(|| re(r"^\s*\[[^\]]*\]\s*"));
static SEASON_EPISODE: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)\bs(\d{1,4}) ?e(\d{1,3})((?:-?e\d{1,3}|-\d{1,3}\b)*)"));
static SEASON_EPISODE_WORDS: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)\bseason (\d{1,4}),? episode (\d{1,3})\b"));
static SEASON_X_EPISODE: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\b(\d{1,2})x(\d{2,3})\b"));
static AIR_DATE: LazyLock<Regex> =
    LazyLock::new(|| re(r"\b((?:19|20)\d{2})[ -](\d{2})[ -](\d{2})\b"));
static YEAR: LazyLock<Regex> = LazyLock::new(|| re(r"\b(19\d{2}|20\d{2})\b"));
static TRAILING_YEAR: LazyLock<Regex> =
    LazyLock::new(|| re(r"^(.+?)[ (\[]+(19\d{2}|20\d{2})[)\]]?\s*$"));
static RESOLUTION: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)\b(?:(2160|1080|720|576|480)[pi]|(4k|uhd))\b"));
static PART: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\b(?:pt|part|cd|dis[ck]) ?(\d)\b"));
static JUNK: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?i)\b(?:2160p|1080[pi]|720p|576p|480p|4k|uhd|blu-?ray|b[dr]rip|dvdrip|web-?dl|webrip|hdtv|hdrip|remux|x26[45]|h ?26[45]|hevc|xvid|divx|aac|ac3|dts|proper|repack|10bit)\b",
    )
});
static SEASON_DIR: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)^(?:(?:season|series|s) ?\d+|specials)$"));
static SPACES: LazyLock<Regex> = LazyLock::new(|| re(r"\s+"));

static EDITIONS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    [
        (r"director'?s cut", "Director's Cut"),
        (r"final cut", "Final Cut"),
        (r"ultimate (?:cut|edition)", "Ultimate Edition"),
        (r"special edition", "Special Edition"),
        (r"extended(?: cut| edition)?", "Extended"),
        (r"theatrical(?: cut| edition)?", "Theatrical"),
        (r"unrated(?: cut| edition)?", "Unrated"),
        (r"uncut", "Uncut"),
        (r"remastered", "Remastered"),
        (r"imax", "IMAX"),
    ]
    .into_iter()
    .map(|(pattern, name)| (re(&format!(r"(?i)\b{pattern}\b")), name))
    .collect()
});

fn max_year() -> i32 {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    1970 + (secs / 31_556_952) as i32 + 1
}

fn clean_title(raw: &str) -> String {
    let collapsed = SPACES.replace_all(raw, " ");
    collapsed
        .trim_matches(|c: char| c.is_whitespace() || matches!(c, '-' | '(' | '[' | ','))
        .to_string()
}

/// Earliest edition marker in `text`, as `(start, canonical name)`.
fn find_edition(text: &str) -> Option<(usize, &'static str)> {
    EDITIONS
        .iter()
        .filter_map(|(regex, name)| regex.find(text).map(|m| (m.start(), *name)))
        .min_by_key(|(start, _)| *start)
}

fn split_trailing_year(title: &str) -> (String, Option<i32>) {
    let title = clean_title(title);
    if let Some(caps) = TRAILING_YEAR.captures(&title)
        && let Ok(year) = caps[2].parse::<i32>()
        && year <= max_year()
    {
        return (clean_title(&caps[1]), Some(year));
    }
    (title, None)
}

struct EpisodeMatch {
    start: usize,
    end: usize,
    season: Option<u32>,
    episode: Option<u32>,
    episode_end: Option<u32>,
    air_date: Option<String>,
}

fn find_episode(text: &str) -> Option<EpisodeMatch> {
    let numbered = |caps: &regex::Captures, tail: Option<&str>| {
        let whole = caps.get(0).unwrap();
        let episode: u32 = caps[2].parse().ok()?;
        let episode_end = tail
            .and_then(|tail| re(r"\d+").find_iter(tail).last())
            .and_then(|m| m.as_str().parse::<u32>().ok())
            .filter(|end| *end > episode);
        Some(EpisodeMatch {
            start: whole.start(),
            end: whole.end(),
            season: caps[1].parse().ok(),
            episode: Some(episode),
            episode_end,
            air_date: None,
        })
    };
    if let Some(caps) = SEASON_EPISODE.captures(text) {
        let tail = caps.get(3).map(|m| m.as_str());
        return numbered(&caps, tail);
    }
    for regex in [&*SEASON_EPISODE_WORDS, &*SEASON_X_EPISODE] {
        if let Some(caps) = regex.captures(text) {
            return numbered(&caps, None);
        }
    }
    let caps = AIR_DATE.captures(text)?;
    let (month, day): (u32, u32) = (caps[2].parse().ok()?, caps[3].parse().ok()?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let whole = caps.get(0).unwrap();
    Some(EpisodeMatch {
        start: whole.start(),
        end: whole.end(),
        season: None,
        episode: None,
        episode_end: None,
        air_date: Some(format!("{}-{month:02}-{day:02}", &caps[1])),
    })
}

/// Parses a single file or directory name (without extension).
pub fn parse_name(name: &str) -> Parsed {
    let mut edition = None;
    let mut id_hint = None;
    for caps in TAG.captures_iter(name) {
        let (key, value) = (caps[1].to_lowercase(), caps[2].trim().to_string());
        match key.as_str() {
            "edition" => edition = Some(value),
            "tmdb" | "tvdb" | "imdb" => id_hint = Some((key, value)),
            _ => {}
        }
    }
    let text = TAG.replace_all(name, " ");
    let text = text.replace(['.', '_'], " ");
    let text = LEADING_GROUP.replace(&text, "");
    let text = SPACES.replace_all(&text, " ").trim().to_string();

    let resolution = RESOLUTION.captures(&text).map(|caps| match caps.get(1) {
        Some(height) => format!("{}p", height.as_str()),
        None => "2160p".to_string(),
    });

    if let Some(found) = find_episode(&text) {
        let (title, year) = split_trailing_year(&text[..found.start]);
        let after = &text[found.end..];
        let cut = JUNK.find(after).map_or(after.len(), |m| m.start());
        let episode_title = Some(clean_title(&after[..cut])).filter(|t| !t.is_empty());
        return Parsed {
            kind: Kind::Episode,
            title,
            year,
            season: found.season,
            episode: found.episode,
            episode_end: found.episode_end,
            air_date: found.air_date,
            episode_title,
            edition: None,
            part: None,
            resolution,
            id_hint,
        };
    }

    let limit = max_year();
    let year_match = YEAR
        .find_iter(&text)
        .filter(|m| m.start() > 0 && m.as_str().parse::<i32>().is_ok_and(|y| y <= limit))
        .last();
    let (title, year, remainder) = match year_match {
        Some(m) => (
            clean_title(&text[..m.start()]),
            m.as_str().parse().ok(),
            &text[m.end()..],
        ),
        None => {
            let cut = [
                JUNK.find(&text).map(|m| m.start()),
                find_edition(&text).map(|(start, _)| start),
                PART.find(&text).map(|m| m.start()),
            ]
            .into_iter()
            .flatten()
            .filter(|start| *start > 0)
            .min()
            .unwrap_or(text.len());
            (clean_title(&text[..cut]), None, &text[cut..])
        }
    };
    let edition = edition.or_else(|| find_edition(remainder).map(|(_, name)| name.to_string()));
    let part = PART
        .captures(remainder)
        .and_then(|caps| caps[1].parse().ok());

    Parsed {
        kind: Kind::Movie,
        title,
        year,
        season: None,
        episode: None,
        episode_end: None,
        air_date: None,
        episode_title: None,
        edition,
        part,
        resolution,
        id_hint,
    }
}

/// Parses a video path, consulting parent directories where the file name alone is not enough.
pub fn parse(path: &Path) -> Parsed {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let mut parsed = parse_name(stem);

    let ancestors: Vec<Parsed> = path
        .ancestors()
        .skip(1)
        .filter_map(|dir| dir.file_name()?.to_str())
        .filter(|name| !SEASON_DIR.is_match(name.trim()))
        .take(2)
        .map(parse_name)
        .collect();

    if parsed.id_hint.is_none() {
        parsed.id_hint = ancestors.iter().find_map(|a| a.id_hint.clone());
    }
    if let Some(parent) = ancestors.first() {
        match parsed.kind {
            Kind::Episode if parsed.title.is_empty() => {
                // A show folder such as "Show (2005)" parses as a film; reuse its title and year.
                parsed.title = parent.title.clone();
                parsed.year = parent.year;
            }
            Kind::Movie
                if parent.kind == Kind::Movie
                    && parent.year.is_some()
                    && (parsed.year.is_none() || parsed.title.is_empty()) =>
            {
                parsed.title = parent.title.clone();
                parsed.year = parent.year;
                parsed.edition = parsed.edition.or_else(|| parent.edition.clone());
            }
            _ => {}
        }
    }
    parsed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn movie(name: &str) -> (String, Option<i32>) {
        let p = parse_name(name);
        assert_eq!(p.kind, Kind::Movie, "{name}");
        (p.title, p.year)
    }

    #[test]
    fn scene_movie() {
        let p = parse_name("The.Matrix.1999.1080p.BluRay.x264-GROUP");
        assert_eq!(p.kind, Kind::Movie);
        assert_eq!(p.title, "The Matrix");
        assert_eq!(p.year, Some(1999));
        assert_eq!(p.resolution.as_deref(), Some("1080p"));
    }

    #[test]
    fn movie_titles_containing_years() {
        assert_eq!(
            movie("Blade.Runner.2049.2017.2160p.UHD"),
            ("Blade Runner 2049".into(), Some(2017))
        );
        assert_eq!(movie("2012 (2009)"), ("2012".into(), Some(2009)));
        assert_eq!(movie("1917.2019.720p"), ("1917".into(), Some(2019)));
        assert_eq!(
            movie("2001 A Space Odyssey (1968)"),
            ("2001 A Space Odyssey".into(), Some(1968))
        );
    }

    #[test]
    fn movie_without_year() {
        assert_eq!(movie("Some_Movie_720p_HDTV"), ("Some Movie".into(), None));
        assert_eq!(movie("Plain Title"), ("Plain Title".into(), None));
    }

    #[test]
    fn plex_style_movie() {
        let p = parse_name("Blade Runner (1982) {tmdb-78} {edition-Final Cut} - pt2");
        assert_eq!(p.title, "Blade Runner");
        assert_eq!(p.year, Some(1982));
        assert_eq!(p.edition.as_deref(), Some("Final Cut"));
        assert_eq!(p.part, Some(2));
        assert_eq!(p.id_hint, Some(("tmdb".into(), "78".into())));
    }

    #[test]
    fn edition_and_part_detection() {
        let p = parse_name("Aliens.1986.Directors.Cut.1080p.CD1");
        assert_eq!(p.title, "Aliens");
        assert_eq!(p.edition.as_deref(), Some("Director's Cut"));
        assert_eq!(p.part, Some(1));

        let p = parse_name("Harry Potter and the Deathly Hallows Part 2 (2011)");
        assert_eq!(p.title, "Harry Potter and the Deathly Hallows Part 2");
        assert_eq!(p.part, None);
    }

    #[test]
    fn standard_episode() {
        let p = parse_name("Breaking.Bad.S02E05.Breakage.720p.BluRay.x264");
        assert_eq!(p.kind, Kind::Episode);
        assert_eq!(p.title, "Breaking Bad");
        assert_eq!(
            (p.season, p.episode, p.episode_end),
            (Some(2), Some(5), None)
        );
        assert_eq!(p.episode_title.as_deref(), Some("Breakage"));
    }

    #[test]
    fn episode_with_show_year() {
        let p = parse_name("Doctor Who (2005) - s01e01 - Rose");
        assert_eq!(p.title, "Doctor Who");
        assert_eq!(p.year, Some(2005));
        assert_eq!(p.episode_title.as_deref(), Some("Rose"));

        let p = parse_name("The.Office.2005.S03E01.720p");
        assert_eq!((p.title.as_str(), p.year), ("The Office", Some(2005)));
    }

    #[test]
    fn multi_episode() {
        for name in [
            "Show S02E01-E03",
            "Show.s02e01e03",
            "Show - S02E01-03 - Title",
        ] {
            let p = parse_name(name);
            assert_eq!(
                (p.season, p.episode, p.episode_end),
                (Some(2), Some(1), Some(3)),
                "{name}"
            );
        }
        let p = parse_name("Show.S01E01-720p");
        assert_eq!(p.episode_end, None);
    }

    #[test]
    fn alternate_episode_forms() {
        let p = parse_name("Show 1x02 Title");
        assert_eq!((p.season, p.episode), (Some(1), Some(2)));
        let p = parse_name("Show Season 3 Episode 4");
        assert_eq!((p.season, p.episode), (Some(3), Some(4)));
        assert_eq!(parse_name("Movie 1920x1080 (2010)").kind, Kind::Movie);
    }

    #[test]
    fn date_based_episode() {
        let p = parse_name("The.Colbert.Report.2011.11.15.Elijah.Wood.HDTV");
        assert_eq!(p.kind, Kind::Episode);
        assert_eq!(p.title, "The Colbert Report");
        assert_eq!(p.air_date.as_deref(), Some("2011-11-15"));
        assert_eq!(p.episode_title.as_deref(), Some("Elijah Wood"));
    }

    #[test]
    fn title_from_directories() {
        let p = parse(Path::new(
            "/tv/Doctor Who (2005) {tvdb-78804}/Season 01/S01E02.mkv",
        ));
        assert_eq!((p.title.as_str(), p.year), ("Doctor Who", Some(2005)));
        assert_eq!(p.id_hint, Some(("tvdb".into(), "78804".into())));

        let p = parse(Path::new("/dl/Heat (1995)/heat-1080p.mkv"));
        assert_eq!((p.title.as_str(), p.year), ("Heat", Some(1995)));

        let p = parse(Path::new("/downloads/Heat.1995.mkv"));
        assert_eq!((p.title.as_str(), p.year), ("Heat", Some(1995)));
    }
}
