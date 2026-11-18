use std::fmt;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, clap::ValueEnum)]
pub enum Kind {
    Movie,
    #[value(alias = "tv")]
    Episode,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Movie => "movie",
            Kind::Episode => "episode",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "movie" => Some(Kind::Movie),
            "episode" => Some(Kind::Episode),
            _ => None,
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioStream {
    pub title: Option<String>,
    pub language: Option<String>,
    pub codec: Option<String>,
    /// Codec profile as reported by ffprobe, such as `DTS-HD MA`.
    pub profile: Option<String>,
    pub channels: Option<u32>,
    pub layout: Option<String>,
    pub atmos: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SubtitleStream {
    pub title: Option<String>,
    pub language: Option<String>,
    pub codec: Option<String>,
    pub forced: bool,
    /// Flagged as being for the deaf and hard of hearing.
    pub sdh: bool,
}

/// Technical properties of a file's primary video stream and of its audio and embedded
/// subtitle streams.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MediaInfo {
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Conventional label such as `1080p`.
    pub resolution: Option<String>,
    pub video_codec: Option<String>,
    pub bit_depth: Option<u32>,
    pub colour_space: Option<String>,
    pub colour_primaries: Option<String>,
    /// Transfer function, such as `smpte2084` (PQ) or `arib-std-b67` (HLG).
    pub transfer: Option<String>,
    /// `SDR`, or a `/`-separated list drawn from `DV`, `HDR10+`, `HDR10`, `HLG`.
    pub hdr: Option<String>,
    /// Dolby Vision profile: `5`, `8.1`, `7 FEL`, `7 MEL`, ...
    pub dv_profile: Option<String>,
    pub duration: Option<f64>,
    /// The container's own title tag, which often names the episode a file holds.
    pub title: Option<String>,
    pub audio: Vec<AudioStream>,
    pub subtitles: Vec<SubtitleStream>,
}

/// One video file in the library. For episodes, `title` and `year` describe the show.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub id: Option<i64>,
    pub kind: Kind,
    pub path: PathBuf,
    /// Where the file was imported from; unknown for items imported before this was recorded.
    pub original_path: Option<PathBuf>,
    pub title: String,
    pub year: Option<i32>,
    pub source: Option<String>,
    pub source_id: Option<String>,
    pub edition: Option<String>,
    pub part: Option<u32>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub episode_end: Option<u32>,
    pub episode_title: Option<String>,
    pub air_date: Option<String>,
    pub date_based: bool,
    pub media: MediaInfo,
    pub size: u64,
    pub mtime: i64,
    pub added: i64,
}

impl Item {
    pub fn new(kind: Kind, path: PathBuf, title: String) -> Self {
        Item {
            id: None,
            kind,
            path,
            original_path: None,
            title,
            year: None,
            source: None,
            source_id: None,
            edition: None,
            part: None,
            season: None,
            episode: None,
            episode_end: None,
            episode_title: None,
            air_date: None,
            date_based: false,
            media: MediaInfo::default(),
            size: 0,
            mtime: 0,
            added: 0,
        }
    }

    /// Field names usable in queries and format strings.
    pub const FIELDS: &[&str] = &[
        "id",
        "kind",
        "path",
        "original_path",
        "title",
        "year",
        "source",
        "source_id",
        "edition",
        "part",
        "season",
        "episode",
        "episode_end",
        "episode_title",
        "air_date",
        "resolution",
        "width",
        "height",
        "video_codec",
        "bit_depth",
        "colour_space",
        "colour_primaries",
        "transfer",
        "hdr",
        "dv_profile",
        "duration",
        "embedded_title",
        "audio_codec",
        "audio_profile",
        "audio_channels",
        "audio_layout",
        "audio_language",
        "audio_title",
        "atmos",
        "subtitle_codec",
        "subtitle_language",
        "subtitle_title",
        "subtitle_forced",
        "subtitle_sdh",
        "size",
    ];

    pub fn field(&self, name: &str) -> Option<String> {
        fn opt<T: ToString>(v: &Option<T>) -> String {
            v.as_ref().map(T::to_string).unwrap_or_default()
        }
        let media = &self.media;
        let audio = |get: fn(&AudioStream) -> String| {
            let values: Vec<_> = media.audio.iter().map(get).collect();
            values.join(", ")
        };
        let subtitles = |get: fn(&SubtitleStream) -> String| {
            let values: Vec<_> = media.subtitles.iter().map(get).collect();
            values.join(", ")
        };
        Some(match name {
            "id" => opt(&self.id),
            "kind" => self.kind.to_string(),
            "path" => self.path.display().to_string(),
            "original_path" => self
                .original_path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            "title" => self.title.clone(),
            "year" => opt(&self.year),
            "source" => opt(&self.source),
            "source_id" => opt(&self.source_id),
            "edition" => opt(&self.edition),
            "part" => opt(&self.part),
            "season" => opt(&self.season),
            "episode" => opt(&self.episode),
            "episode_end" => opt(&self.episode_end),
            "episode_title" => opt(&self.episode_title),
            "air_date" => opt(&self.air_date),
            "resolution" => opt(&media.resolution),
            "width" => opt(&media.width),
            "height" => opt(&media.height),
            "video_codec" => opt(&media.video_codec),
            "bit_depth" => opt(&media.bit_depth),
            "colour_space" => opt(&media.colour_space),
            "colour_primaries" => opt(&media.colour_primaries),
            "transfer" => opt(&media.transfer),
            "hdr" => opt(&media.hdr),
            "dv_profile" => opt(&media.dv_profile),
            "duration" => media
                .duration
                .map(|d| format!("{d:.0}"))
                .unwrap_or_default(),
            "embedded_title" => opt(&media.title),
            "audio_codec" => audio(|a| opt(&a.codec)),
            "audio_profile" => audio(|a| opt(&a.profile)),
            "audio_channels" => audio(|a| opt(&a.channels)),
            "audio_layout" => audio(|a| opt(&a.layout)),
            "audio_language" => audio(|a| opt(&a.language)),
            "audio_title" => audio(|a| opt(&a.title)),
            "atmos" => media.audio.iter().any(|a| a.atmos).to_string(),
            "subtitle_codec" => subtitles(|s| opt(&s.codec)),
            "subtitle_language" => subtitles(|s| opt(&s.language)),
            "subtitle_title" => subtitles(|s| opt(&s.title)),
            "subtitle_forced" => media.subtitles.iter().any(|s| s.forced).to_string(),
            "subtitle_sdh" => media.subtitles.iter().any(|s| s.sdh).to_string(),
            "size" => self.size.to_string(),
            _ => return None,
        })
    }

    /// Expands `$field` and `${field}` references.
    pub fn format(&self, template: &str) -> String {
        let mut out = String::new();
        let mut rest = template;
        while let Some(pos) = rest.find('$') {
            out.push_str(&rest[..pos]);
            let after = &rest[pos + 1..];
            let (name, consumed) = if let Some(braced) = after.strip_prefix('{') {
                match braced.find('}') {
                    Some(end) => (&braced[..end], end + 2),
                    None => ("", 0),
                }
            } else {
                let end = after
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .unwrap_or(after.len());
                (&after[..end], end)
            };
            match self.field(name) {
                Some(value) => {
                    out.push_str(&value);
                    rest = &after[consumed..];
                }
                None => {
                    out.push('$');
                    rest = after;
                }
            }
        }
        out.push_str(rest);
        out
    }

    /// Whether `other` is another copy (version) of the same film or episode.
    pub fn same_work(&self, other: &Item) -> bool {
        self.kind == other.kind
            && self.title == other.title
            && self.year == other.year
            && self.source_id == other.source_id
            && self.edition == other.edition
            && self.part == other.part
            && self.season == other.season
            && self.episode == other.episode
            && self.air_date == other.air_date
    }

    /// Whether `other` is the same film or episode at the same resolution and size.
    pub fn is_duplicate_of(&self, other: &Item) -> bool {
        self.same_work(other)
            && self.size == other.size
            && self.media.resolution == other.media.resolution
    }

    /// The fields this item needs to be identified and named that it lacks.
    pub fn missing_metadata(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if self.source.is_none() || self.source_id.is_none() {
            missing.push("source");
        }
        match self.kind {
            Kind::Movie if self.year.is_none() => missing.push("year"),
            Kind::Movie => {}
            Kind::Episode if self.date_based => {
                if self.air_date.is_none() {
                    missing.push("air date");
                }
            }
            Kind::Episode => {
                if self.season.is_none() {
                    missing.push("season");
                }
                if self.episode.is_none() {
                    missing.push("episode");
                }
            }
        }
        missing
    }

    pub fn episode_code(&self) -> Option<String> {
        if self.date_based {
            return self.air_date.clone();
        }
        let (season, episode) = (self.season?, self.episode?);
        Some(match self.episode_end {
            Some(end) if end != episode => format!("s{season:02}e{episode:02}-e{end:02}"),
            _ => format!("s{season:02}e{episode:02}"),
        })
    }
}

impl fmt::Display for Item {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.title)?;
        if let Some(year) = self.year {
            write!(f, " ({year})")?;
        }
        if let Some(code) = self.episode_code() {
            write!(f, " - {code}")?;
        }
        if let Some(title) = &self.episode_title {
            write!(f, " - {title}")?;
        }
        if let Some(edition) = &self.edition {
            write!(f, " [{edition}]")?;
        }
        if let Some(part) = self.part {
            write!(f, " (part {part})")?;
        }
        let tags: Vec<&str> = [&self.media.resolution, &self.media.hdr]
            .into_iter()
            .flatten()
            .map(String::as_str)
            .filter(|tag| *tag != "SDR")
            .collect();
        if !tags.is_empty() {
            write!(f, " [{}]", tags.join(" "))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn movie() -> Item {
        let mut item = Item::new(Kind::Movie, "/m/heat.mkv".into(), "Heat".into());
        item.year = Some(1995);
        item.source = Some("tmdb".into());
        item.source_id = Some("949".into());
        item.size = 1_000;
        item.media.resolution = Some("2160p".into());
        item.media.hdr = Some("DV/HDR10".into());
        item.media.duration = Some(10_200.4);
        item.media.audio = vec![
            AudioStream {
                codec: Some("truehd".into()),
                channels: Some(8),
                atmos: true,
                ..AudioStream::default()
            },
            AudioStream {
                codec: Some("ac3".into()),
                channels: Some(6),
                ..AudioStream::default()
            },
        ];
        item.media.subtitles = vec![SubtitleStream {
            language: Some("eng".into()),
            forced: true,
            ..SubtitleStream::default()
        }];
        item
    }

    fn episode() -> Item {
        let mut item = Item::new(Kind::Episode, "/tv/rose.mkv".into(), "Doctor Who".into());
        item.season = Some(1);
        item.episode = Some(1);
        item
    }

    #[test]
    fn kinds_round_trip_through_text() {
        for kind in [Kind::Movie, Kind::Episode] {
            assert_eq!(Kind::parse(kind.as_str()), Some(kind));
            assert_eq!(kind.to_string(), kind.as_str());
        }
        assert_eq!(Kind::parse("series"), None);
    }

    #[test]
    fn fields_render_as_text() {
        let item = movie();
        assert_eq!(item.field("title").unwrap(), "Heat");
        assert_eq!(item.field("year").unwrap(), "1995");
        assert_eq!(item.field("kind").unwrap(), "movie");
        assert_eq!(item.field("source_id").unwrap(), "949");
        assert_eq!(item.field("duration").unwrap(), "10200");
        assert_eq!(item.field("audio_codec").unwrap(), "truehd, ac3");
        assert_eq!(item.field("audio_channels").unwrap(), "8, 6");
        assert_eq!(item.field("atmos").unwrap(), "true");
        assert_eq!(item.field("subtitle_forced").unwrap(), "true");
        assert_eq!(item.field("subtitle_sdh").unwrap(), "false");
        assert_eq!(item.field("season").unwrap(), "", "unset fields are empty");
        assert_eq!(item.field("nonsense"), None);
    }

    #[test]
    fn every_listed_field_resolves() {
        let item = Item::new(Kind::Movie, "/m/x.mkv".into(), "X".into());
        for name in Item::FIELDS {
            assert!(item.field(name).is_some(), "{name}");
        }
    }

    #[test]
    fn templates_expand_bare_and_braced_references() {
        let item = movie();
        assert_eq!(item.format("$title ($year)"), "Heat (1995)");
        assert_eq!(item.format("${title}s"), "Heats");
        assert_eq!(item.format("$title$year"), "Heat1995");
        assert_eq!(item.format("$resolution/$hdr"), "2160p/DV/HDR10");
        assert_eq!(item.format("no references"), "no references");
        assert_eq!(item.format(""), "");
    }

    #[test]
    fn templates_keep_what_they_cannot_expand() {
        let item = movie();
        assert_eq!(item.format("$nonsense"), "$nonsense");
        assert_eq!(item.format("costs $5"), "costs $5");
        assert_eq!(item.format("${title"), "${title");
        assert_eq!(item.format("trailing $"), "trailing $");
        assert_eq!(item.format("$$title"), "$Heat");
    }

    #[test]
    fn episode_codes() {
        let mut item = episode();
        assert_eq!(item.episode_code().unwrap(), "s01e01");
        item.episode_end = Some(1);
        assert_eq!(
            item.episode_code().unwrap(),
            "s01e01",
            "a one-episode range"
        );
        item.episode_end = Some(3);
        assert_eq!(item.episode_code().unwrap(), "s01e01-e03");
        item.season = Some(12);
        item.episode = Some(104);
        item.episode_end = None;
        assert_eq!(item.episode_code().unwrap(), "s12e104");

        item.date_based = true;
        item.air_date = Some("2011-11-15".into());
        assert_eq!(item.episode_code().unwrap(), "2011-11-15");

        assert_eq!(movie().episode_code(), None);
        item.date_based = false;
        item.season = None;
        assert_eq!(item.episode_code(), None, "needs a season and an episode");
    }

    #[test]
    fn versions_of_the_same_work() {
        let a = movie();
        let mut b = movie();
        b.path = "/m/heat-1080p.mkv".into();
        b.size = 500;
        b.media.resolution = Some("1080p".into());
        assert!(a.same_work(&b));
        assert!(!a.is_duplicate_of(&b), "different size and resolution");

        let mut c = a.clone();
        c.path = "/m/copy.mkv".into();
        assert!(a.is_duplicate_of(&c));

        let mut cut = movie();
        cut.edition = Some("Director's Cut".into());
        assert!(!a.same_work(&cut));
        let mut part = movie();
        part.part = Some(2);
        assert!(!a.same_work(&part));
        let mut remake = movie();
        remake.year = Some(2021);
        assert!(!a.same_work(&remake));
        let mut other_id = movie();
        other_id.source_id = Some("1".into());
        assert!(!a.same_work(&other_id));
    }

    #[test]
    fn episodes_of_a_show_are_distinct_works() {
        let one = episode();
        let mut two = episode();
        two.episode = Some(2);
        assert!(!one.same_work(&two));
        let mut film = movie();
        film.title = one.title.clone();
        assert!(!one.same_work(&film));
    }

    #[test]
    fn display_summarises_an_item() {
        let mut item = movie();
        assert_eq!(item.to_string(), "Heat (1995) [2160p DV/HDR10]");
        item.edition = Some("Final Cut".into());
        item.part = Some(1);
        assert_eq!(
            item.to_string(),
            "Heat (1995) [Final Cut] (part 1) [2160p DV/HDR10]"
        );

        let mut show = episode();
        show.episode_title = Some("Rose".into());
        show.media.hdr = Some("SDR".into());
        assert_eq!(show.to_string(), "Doctor Who - s01e01 - Rose");
        show.media.resolution = Some("1080p".into());
        assert_eq!(show.to_string(), "Doctor Who - s01e01 - Rose [1080p]");
    }
}
