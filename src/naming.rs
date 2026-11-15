//! Builds library paths following Plex's naming conventions.

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::model::{Item, Kind};

pub const SUBTITLE_EXTENSIONS: &[&str] = &["srt", "ass", "ssa", "sub", "idx", "vtt", "smi", "sup"];

/// Makes `name` safe as a single path component on common filesystems and network shares.
pub fn sanitise(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        match c {
            ':' => out.push_str(" -"),
            '/' | '\\' => out.push('-'),
            '<' | '>' | '"' | '|' | '?' | '*' => {}
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    let collapsed = out.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.trim_matches(['.', ' ']).to_string()
}

fn base_name(item: &Item, id_tags: bool) -> (String, String) {
    let mut plain = sanitise(&item.title);
    if let Some(year) = item.year {
        plain.push_str(&format!(" ({year})"));
    }
    let tagged = match (&item.source, &item.source_id) {
        (Some(source), Some(id)) if id_tags => format!("{plain} {{{source}-{id}}}"),
        _ => plain.clone(),
    };
    (plain, tagged)
}

/// Path of `item` relative to its library root. `version` distinguishes multiple copies of the
/// same film or episode (for example `1080p`).
pub fn relative_path(
    item: &Item,
    id_tags: bool,
    version: Option<&str>,
    extension: &str,
) -> PathBuf {
    let (plain, tagged) = base_name(item, id_tags);
    let mut path = PathBuf::from(&tagged);
    let mut file = match item.kind {
        Kind::Movie => {
            let mut file = tagged;
            if let Some(edition) = &item.edition {
                file.push_str(&format!(" {{edition-{}}}", sanitise(edition)));
            }
            file
        }
        Kind::Episode => {
            let season = item.season.map_or_else(
                || match &item.air_date {
                    Some(date) => format!("Season {}", &date[..4.min(date.len())]),
                    None => "Season 01".to_string(),
                },
                |season| format!("Season {season:02}"),
            );
            path.push(season);
            let mut file = plain;
            if let Some(code) = item.episode_code() {
                file.push_str(&format!(" - {code}"));
            }
            if let Some(title) = item.episode_title.as_deref().map(sanitise)
                && !title.is_empty()
            {
                file.push_str(&format!(" - {title}"));
            }
            file
        }
    };
    if let Some(version) = version {
        file.push_str(&format!(" - {}", sanitise(version)));
    }
    if let Some(part) = item.part {
        file.push_str(&format!(" - pt{part}"));
    }
    if !extension.is_empty() {
        file.push('.');
        file.push_str(extension);
    }
    path.push(file);
    path
}

pub fn library_root(config: &Config, kind: Kind) -> &Path {
    match kind {
        Kind::Movie => &config.library.movies,
        Kind::Episode => &config.library.tv,
    }
}

pub fn destination(config: &Config, item: &Item, version: Option<&str>) -> PathBuf {
    let extension = item
        .path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    library_root(config, item.kind).join(relative_path(
        item,
        config.naming.id_tags,
        version,
        &extension,
    ))
}

/// Subtitle files belonging to `video`, each with the suffix (such as `.en.forced.srt`) that
/// follows the video's stem.
pub fn sidecars(video: &Path) -> Vec<(PathBuf, String)> {
    let (Some(dir), Some(stem)) = (video.parent(), video.file_stem().and_then(|s| s.to_str()))
    else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<_> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?;
            let suffix = name.strip_prefix(stem)?;
            let extension = path.extension()?.to_str()?.to_lowercase();
            (suffix.starts_with('.')
                && SUBTITLE_EXTENSIONS.contains(&extension.as_str())
                && path.is_file())
            .then(|| (path.clone(), suffix.to_lowercase()))
        })
        .collect();
    found.sort();
    found
}

/// Where a sidecar with `suffix` goes once its video lives at `video_destination`.
pub fn sidecar_destination(video_destination: &Path, suffix: &str) -> PathBuf {
    let mut name = video_destination
        .file_stem()
        .unwrap_or_default()
        .to_os_string();
    name.push(suffix);
    video_destination.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn movie() -> Item {
        let mut item = Item::new(Kind::Movie, "/x/in.MKV".into(), "Blade Runner".into());
        item.year = Some(1982);
        item.source = Some("tmdb".into());
        item.source_id = Some("78".into());
        item
    }

    fn episode() -> Item {
        let mut item = Item::new(Kind::Episode, "/x/in.mkv".into(), "Grey's Anatomy".into());
        item.year = Some(2005);
        item.source = Some("tvdb".into());
        item.source_id = Some("73762".into());
        item.season = Some(2);
        item.episode = Some(1);
        item
    }

    fn rel(item: &Item, tags: bool, version: Option<&str>) -> String {
        relative_path(item, tags, version, "mkv")
            .to_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn movie_paths() {
        let mut item = movie();
        assert_eq!(
            rel(&item, true, None),
            "Blade Runner (1982) {tmdb-78}/Blade Runner (1982) {tmdb-78}.mkv"
        );
        assert_eq!(
            rel(&item, false, None),
            "Blade Runner (1982)/Blade Runner (1982).mkv"
        );
        item.edition = Some("Final Cut".into());
        item.part = Some(1);
        assert_eq!(
            rel(&item, false, Some("1080p")),
            "Blade Runner (1982)/Blade Runner (1982) {edition-Final Cut} - 1080p - pt1.mkv"
        );
    }

    #[test]
    fn episode_paths() {
        let mut item = episode();
        item.episode_title = Some("Raindrops Keep Falling on My Head".into());
        assert_eq!(
            rel(&item, true, None),
            "Grey's Anatomy (2005) {tvdb-73762}/Season 02/Grey's Anatomy (2005) - s02e01 - Raindrops Keep Falling on My Head.mkv"
        );
        item.episode_title = None;
        item.episode_end = Some(3);
        assert_eq!(
            rel(&item, false, None),
            "Grey's Anatomy (2005)/Season 02/Grey's Anatomy (2005) - s02e01-e03.mkv"
        );
    }

    #[test]
    fn specials_and_dates() {
        let mut item = episode();
        item.season = Some(0);
        assert!(rel(&item, false, None).contains("/Season 00/"));

        item.season = Some(8);
        item.date_based = true;
        item.air_date = Some("2011-11-15".into());
        item.episode_title = Some("Elijah Wood".into());
        assert_eq!(
            rel(&item, false, None),
            "Grey's Anatomy (2005)/Season 08/Grey's Anatomy (2005) - 2011-11-15 - Elijah Wood.mkv"
        );
    }

    #[test]
    fn sanitises_components() {
        assert_eq!(sanitise("Mission: Impossible"), "Mission - Impossible");
        assert_eq!(sanitise("Face/Off"), "Face-Off");
        assert_eq!(sanitise("What If...?"), "What If");
    }

    #[test]
    fn sidecar_names() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "film.mkv",
            "film.en.srt",
            "film.EN.Forced.srt",
            "film 2.srt",
            "film.nfo",
        ] {
            std::fs::write(dir.path().join(name), "").unwrap();
        }
        let suffixes: Vec<_> = sidecars(&dir.path().join("film.mkv"))
            .into_iter()
            .map(|(_, suffix)| suffix)
            .collect();
        assert_eq!(suffixes, [".en.forced.srt", ".en.srt"]);
        assert_eq!(
            sidecar_destination(Path::new("/lib/A (2000)/A (2000).mkv"), ".en.srt"),
            Path::new("/lib/A (2000)/A (2000).en.srt")
        );
    }
}
