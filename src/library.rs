//! The SQLite-backed library of imported files.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, Row, params, params_from_iter};

use crate::model::{AudioStream, Item, Kind, MediaInfo, SubtitleStream};
use crate::query::Query;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS items (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    path TEXT NOT NULL UNIQUE,
    original_path TEXT,
    title TEXT NOT NULL,
    year INTEGER,
    source TEXT,
    source_id TEXT,
    edition TEXT,
    part INTEGER,
    season INTEGER,
    episode INTEGER,
    episode_end INTEGER,
    episode_title TEXT,
    air_date TEXT,
    date_based INTEGER NOT NULL DEFAULT 0,
    width INTEGER,
    height INTEGER,
    resolution TEXT,
    video_codec TEXT,
    bit_depth INTEGER,
    colour_space TEXT,
    colour_primaries TEXT,
    transfer TEXT,
    hdr TEXT,
    dv_profile TEXT,
    duration REAL,
    size INTEGER NOT NULL,
    mtime INTEGER NOT NULL,
    added INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS audio_streams (
    item_id INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    idx INTEGER NOT NULL,
    title TEXT,
    language TEXT,
    codec TEXT,
    profile TEXT,
    channels INTEGER,
    layout TEXT,
    atmos INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (item_id, idx)
);
CREATE TABLE IF NOT EXISTS subtitle_streams (
    item_id INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    idx INTEGER NOT NULL,
    title TEXT,
    language TEXT,
    codec TEXT,
    forced INTEGER NOT NULL DEFAULT 0,
    sdh INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (item_id, idx)
);
";

const COLUMNS: &str = "kind, path, original_path, title, year, source, source_id, edition, part, season, episode, \
    episode_end, episode_title, air_date, date_based, width, height, resolution, video_codec, \
    bit_depth, colour_space, colour_primaries, transfer, hdr, dv_profile, duration, size, mtime, \
    added";

pub struct Library {
    conn: Connection,
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn item_from_row(row: &Row) -> rusqlite::Result<Item> {
    let kind: String = row.get("kind")?;
    let path: String = row.get("path")?;
    Ok(Item {
        id: row.get("id")?,
        kind: Kind::parse(&kind).unwrap_or(Kind::Movie),
        path: PathBuf::from(path),
        original_path: row
            .get::<_, Option<String>>("original_path")?
            .map(PathBuf::from),
        title: row.get("title")?,
        year: row.get("year")?,
        source: row.get("source")?,
        source_id: row.get("source_id")?,
        edition: row.get("edition")?,
        part: row.get("part")?,
        season: row.get("season")?,
        episode: row.get("episode")?,
        episode_end: row.get("episode_end")?,
        episode_title: row.get("episode_title")?,
        air_date: row.get("air_date")?,
        date_based: row.get("date_based")?,
        media: MediaInfo {
            width: row.get("width")?,
            height: row.get("height")?,
            resolution: row.get("resolution")?,
            video_codec: row.get("video_codec")?,
            bit_depth: row.get("bit_depth")?,
            colour_space: row.get("colour_space")?,
            colour_primaries: row.get("colour_primaries")?,
            transfer: row.get("transfer")?,
            hdr: row.get("hdr")?,
            dv_profile: row.get("dv_profile")?,
            duration: row.get("duration")?,
            audio: Vec::new(),
            subtitles: Vec::new(),
        },
        size: row.get::<_, i64>("size")? as u64,
        mtime: row.get("mtime")?,
        added: row.get("added")?,
    })
}

impl Library {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("cannot open library database {}", path.display()))?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.execute_batch(SCHEMA)?;
        Self::add_missing_columns(&conn)?;
        Ok(Library { conn })
    }

    /// Brings a database created by an older version up to the current schema.
    fn add_missing_columns(conn: &Connection) -> Result<()> {
        let has_original_path: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM pragma_table_info('items') WHERE name = 'original_path')",
            [],
            |row| row.get(0),
        )?;
        if !has_original_path {
            conn.execute("ALTER TABLE items ADD COLUMN original_path TEXT", [])?;
        }
        Ok(())
    }

    pub fn contains_path(&self, path: &Path) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM items WHERE path = ?1)",
            [path_text(path)],
            |row| row.get(0),
        )?)
    }

    /// Inserts `item`, or updates it when it already has an id.
    pub fn save(&mut self, item: &mut Item) -> Result<()> {
        let tx = self.conn.transaction()?;
        let media = &item.media;
        let values = params![
            item.kind.as_str(),
            path_text(&item.path),
            item.original_path.as_deref().map(path_text),
            item.title,
            item.year,
            item.source,
            item.source_id,
            item.edition,
            item.part,
            item.season,
            item.episode,
            item.episode_end,
            item.episode_title,
            item.air_date,
            item.date_based,
            media.width,
            media.height,
            media.resolution,
            media.video_codec,
            media.bit_depth,
            media.colour_space,
            media.colour_primaries,
            media.transfer,
            media.hdr,
            media.dv_profile,
            media.duration,
            item.size as i64,
            item.mtime,
            item.added,
            item.id,
        ];
        let placeholders: Vec<String> = (1..=29).map(|n| format!("?{n}")).collect();
        match item.id {
            None => {
                tx.execute(
                    &format!(
                        "INSERT INTO items ({COLUMNS}, id) VALUES ({}, ?30)",
                        placeholders.join(", ")
                    ),
                    values,
                )?;
                item.id = Some(tx.last_insert_rowid());
            }
            Some(_) => {
                tx.execute(
                    &format!(
                        "UPDATE items SET ({COLUMNS}) = ({}) WHERE id = ?30",
                        placeholders.join(", ")
                    ),
                    values,
                )?;
            }
        }
        tx.execute("DELETE FROM audio_streams WHERE item_id = ?1", [item.id])?;
        for (idx, stream) in item.media.audio.iter().enumerate() {
            tx.execute(
                "INSERT INTO audio_streams \
                 (item_id, idx, title, language, codec, profile, channels, layout, atmos) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    item.id,
                    idx as i64,
                    stream.title,
                    stream.language,
                    stream.codec,
                    stream.profile,
                    stream.channels,
                    stream.layout,
                    stream.atmos,
                ],
            )?;
        }
        tx.execute("DELETE FROM subtitle_streams WHERE item_id = ?1", [item.id])?;
        for (idx, stream) in item.media.subtitles.iter().enumerate() {
            tx.execute(
                "INSERT INTO subtitle_streams (item_id, idx, title, language, codec, forced, sdh) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    item.id,
                    idx as i64,
                    stream.title,
                    stream.language,
                    stream.codec,
                    stream.forced,
                    stream.sdh,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn remove(&mut self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM items WHERE id = ?1", [id])?;
        Ok(())
    }

    fn audio_streams(&self, id: i64) -> Result<Vec<AudioStream>> {
        let mut statement = self.conn.prepare_cached(
            "SELECT title, language, codec, profile, channels, layout, atmos \
             FROM audio_streams WHERE item_id = ?1 ORDER BY idx",
        )?;
        let streams = statement
            .query_map([id], |row| {
                Ok(AudioStream {
                    title: row.get(0)?,
                    language: row.get(1)?,
                    codec: row.get(2)?,
                    profile: row.get(3)?,
                    channels: row.get(4)?,
                    layout: row.get(5)?,
                    atmos: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(streams)
    }

    fn subtitle_streams(&self, id: i64) -> Result<Vec<SubtitleStream>> {
        let mut statement = self.conn.prepare_cached(
            "SELECT title, language, codec, forced, sdh \
             FROM subtitle_streams WHERE item_id = ?1 ORDER BY idx",
        )?;
        let streams = statement
            .query_map([id], |row| {
                Ok(SubtitleStream {
                    title: row.get(0)?,
                    language: row.get(1)?,
                    codec: row.get(2)?,
                    forced: row.get(3)?,
                    sdh: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(streams)
    }

    pub fn query(&self, query: &Query) -> Result<Vec<Item>> {
        let sql = format!(
            "SELECT * FROM items WHERE {} \
             ORDER BY kind DESC, title COLLATE NOCASE, year, season, episode, air_date, part, path",
            query.where_clause()
        );
        let mut statement = self.conn.prepare(&sql)?;
        let mut items: Vec<Item> = statement
            .query_map(params_from_iter(query.params()), item_from_row)?
            .collect::<rusqlite::Result<_>>()?;
        for item in &mut items {
            if let Some(id) = item.id {
                item.media.audio = self.audio_streams(id)?;
                item.media.subtitles = self.subtitle_streams(id)?;
            }
        }
        Ok(items)
    }

    /// Whether another library file is a different version of the same film or episode.
    pub fn has_other_versions(&self, item: &Item) -> Result<bool> {
        self.same_work_exists(item, false)
    }

    /// Whether another library file is the same film or episode with the same resolution and
    /// size, which is to say a copy of this very file.
    pub fn has_duplicate(&self, item: &Item) -> Result<bool> {
        self.same_work_exists(item, true)
    }

    fn same_work_exists(&self, item: &Item, same_file: bool) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM items WHERE kind = ?1 AND title = ?2 AND year IS ?3 \
             AND source_id IS ?4 AND edition IS ?5 AND part IS ?6 AND season IS ?7 \
             AND episode IS ?8 AND air_date IS ?9 AND id IS NOT ?10 \
             AND (NOT ?11 OR (size = ?12 AND resolution IS ?13)))",
            params![
                item.kind.as_str(),
                item.title,
                item.year,
                item.source_id,
                item.edition,
                item.part,
                item.season,
                item.episode,
                item.air_date,
                item.id,
                same_file,
                item.size as i64,
                item.media.resolution,
            ],
            |row| row.get(0),
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<Item> {
        let mut matrix = Item::new(Kind::Movie, "/m/matrix.mkv".into(), "The Matrix".into());
        matrix.year = Some(1999);
        matrix.media.resolution = Some("2160p".into());
        matrix.media.hdr = Some("DV/HDR10+/HDR10".into());
        matrix.media.dv_profile = Some("7 FEL".into());
        matrix.media.subtitles = vec![SubtitleStream {
            language: Some("eng".into()),
            codec: Some("subrip".into()),
            forced: true,
            ..SubtitleStream::default()
        }];
        matrix.media.audio = vec![
            AudioStream {
                codec: Some("truehd".into()),
                channels: Some(8),
                atmos: true,
                ..AudioStream::default()
            },
            AudioStream {
                codec: Some("ac3".into()),
                channels: Some(6),
                title: Some("Commentary".into()),
                ..AudioStream::default()
            },
        ];

        let mut heat = Item::new(Kind::Movie, "/m/heat.mkv".into(), "Heat".into());
        heat.year = Some(1995);
        heat.media.hdr = Some("HDR10".into());
        heat.media.audio = vec![AudioStream {
            codec: Some("dts".into()),
            channels: Some(6),
            ..AudioStream::default()
        }];

        let mut rose = Item::new(Kind::Episode, "/t/rose.mkv".into(), "Doctor Who".into());
        rose.year = Some(2005);
        rose.season = Some(1);
        rose.episode = Some(1);
        rose.episode_title = Some("Rose".into());
        rose.media.hdr = Some("SDR".into());
        vec![matrix, heat, rose]
    }

    fn library() -> Library {
        let mut library = Library::in_memory().unwrap();
        for mut item in sample() {
            library.save(&mut item).unwrap();
        }
        library
    }

    fn titles(library: &Library, terms: &[&str]) -> Vec<String> {
        library
            .query(&Query::parse(terms).unwrap())
            .unwrap()
            .into_iter()
            .map(|item| item.title)
            .collect()
    }

    #[test]
    fn round_trip() {
        let mut library = Library::in_memory().unwrap();
        let mut item = sample().remove(0);
        library.save(&mut item).unwrap();
        assert!(item.id.is_some());
        assert_eq!(library.query(&Query::default()).unwrap(), [item.clone()]);

        item.media.audio.pop();
        item.path = "/m/moved.mkv".into();
        library.save(&mut item).unwrap();
        assert_eq!(library.query(&Query::default()).unwrap(), [item.clone()]);
        assert!(library.contains_path(Path::new("/m/moved.mkv")).unwrap());

        library.remove(item.id.unwrap()).unwrap();
        assert!(library.query(&Query::default()).unwrap().is_empty());
    }

    #[test]
    fn queries() {
        let library = library();
        assert_eq!(titles(&library, &[]), ["Heat", "The Matrix", "Doctor Who"]);
        assert_eq!(titles(&library, &["matrix"]), ["The Matrix"]);
        assert_eq!(titles(&library, &["rose"]), ["Doctor Who"]);
        assert_eq!(
            titles(&library, &["kind:movie", "year:1990..1996"]),
            ["Heat"]
        );
        assert_eq!(titles(&library, &["year:..1999", "^heat"]), ["The Matrix"]);
        assert_eq!(titles(&library, &["season:1"]), ["Doctor Who"]);
        assert_eq!(titles(&library, &["title:=heat"]), ["Heat"]);
    }

    #[test]
    fn media_queries() {
        let library = library();
        assert_eq!(titles(&library, &["hdr:hdr10"]), ["Heat", "The Matrix"]);
        assert_eq!(titles(&library, &["hdr:hdr10+"]), ["The Matrix"]);
        assert_eq!(titles(&library, &["hdr:dv"]), ["The Matrix"]);
        assert_eq!(titles(&library, &["hdr:sdr"]), ["Doctor Who"]);
        assert_eq!(titles(&library, &["dv_profile:fel"]), ["The Matrix"]);
        assert_eq!(titles(&library, &["atmos:true"]), ["The Matrix"]);
        assert_eq!(titles(&library, &["^atmos:true"]), ["Heat", "Doctor Who"]);
        assert_eq!(titles(&library, &["audio_codec:dts"]), ["Heat"]);
        assert_eq!(titles(&library, &["audio_channels:8"]), ["The Matrix"]);
        assert_eq!(
            titles(&library, &["audio_title:commentary"]),
            ["The Matrix"]
        );
        assert_eq!(titles(&library, &["subtitle_language:eng"]), ["The Matrix"]);
        assert_eq!(titles(&library, &["subtitle_forced:true"]), ["The Matrix"]);
        assert!(Query::parse(&["year:abc"]).is_err());
    }

    #[test]
    fn other_versions() {
        let mut library = library();
        let mut copy = sample().remove(1);
        copy.path = "/m/heat-4k.mkv".into();
        assert!(library.has_other_versions(&copy).unwrap());
        copy.edition = Some("Director's Cut".into());
        assert!(!library.has_other_versions(&copy).unwrap());

        let mut existing = library
            .query(&Query::parse(&["heat"]).unwrap())
            .unwrap()
            .remove(0);
        assert!(!library.has_other_versions(&existing).unwrap());
        library.save(&mut existing).unwrap();
    }

    #[test]
    fn original_path_round_trips() {
        let mut library = Library::in_memory().unwrap();
        let mut item = sample().remove(0);
        item.original_path = Some("/downloads/matrix.mkv".into());
        library.save(&mut item).unwrap();
        let found = library.query(&Query::parse(&["matrix"]).unwrap()).unwrap();
        assert_eq!(found[0].original_path, item.original_path);
    }

    #[test]
    fn upgrades_database_without_original_path() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&SCHEMA.replace("    original_path TEXT,\n", ""))
            .unwrap();
        let mut library = Library::init(conn).unwrap();
        let mut item = sample().remove(0);
        item.original_path = Some("/downloads/matrix.mkv".into());
        library.save(&mut item).unwrap();
    }

    #[test]
    fn duplicates() {
        let library = library();
        let mut copy = library
            .query(&Query::parse(&["heat"]).unwrap())
            .unwrap()
            .remove(0);
        copy.id = None;
        copy.path = "/elsewhere/heat.mkv".into();
        assert!(library.has_duplicate(&copy).unwrap());
        copy.size += 1;
        assert!(!library.has_duplicate(&copy).unwrap());
        assert!(library.has_other_versions(&copy).unwrap());
    }
}
