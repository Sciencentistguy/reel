//! beets-style queries: `field:value` terms, numeric ranges, bare words and `^` negation.

use anyhow::{Result, bail};
use rusqlite::types::Value;

#[derive(Debug, Clone, Copy, PartialEq)]
enum FieldKind {
    Text,
    Number,
    /// `/`-separated list matched element-wise, so `hdr:hdr10` does not match `HDR10+`.
    List,
    Flag,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Field {
    column: &'static str,
    kind: FieldKind,
    /// Per-stream table the column lives in, when it is not a column of `items`.
    table: Option<&'static str>,
}

fn field(name: &str) -> Option<Field> {
    use FieldKind::*;
    let item = |column, kind| Field {
        column,
        kind,
        table: None,
    };
    let audio = |column, kind| Field {
        column,
        kind,
        table: Some("audio_streams"),
    };
    let subtitle = |column, kind| Field {
        column,
        kind,
        table: Some("subtitle_streams"),
    };
    Some(match name {
        "kind" => item("kind", Text),
        "path" => item("path", Text),
        "title" => item("title", Text),
        "source" => item("source", Text),
        "source_id" => item("source_id", Text),
        "edition" => item("edition", Text),
        "episode_title" => item("episode_title", Text),
        "air_date" => item("air_date", Text),
        "resolution" => item("resolution", Text),
        "video_codec" => item("video_codec", Text),
        "colour_space" => item("colour_space", Text),
        "colour_primaries" => item("colour_primaries", Text),
        "transfer" => item("transfer", Text),
        "dv_profile" => item("dv_profile", Text),
        "hdr" => item("hdr", List),
        "id" => item("id", Number),
        "year" => item("year", Number),
        "part" => item("part", Number),
        "season" => item("season", Number),
        "episode" => item("episode", Number),
        "episode_end" => item("episode_end", Number),
        "width" => item("width", Number),
        "height" => item("height", Number),
        "bit_depth" => item("bit_depth", Number),
        "duration" => item("duration", Number),
        "size" => item("size", Number),
        "audio_codec" => audio("codec", Text),
        "audio_profile" => audio("profile", Text),
        "audio_layout" => audio("layout", Text),
        "audio_language" => audio("language", Text),
        "audio_title" => audio("title", Text),
        "audio_channels" => audio("channels", Number),
        "atmos" => audio("atmos", Flag),
        "subtitle_codec" => subtitle("codec", Text),
        "subtitle_language" => subtitle("language", Text),
        "subtitle_title" => subtitle("title", Text),
        "subtitle_forced" => subtitle("forced", Flag),
        "subtitle_sdh" => subtitle("sdh", Flag),
        _ => return None,
    })
}

#[derive(Debug, Default)]
pub struct Query {
    clauses: Vec<String>,
    params: Vec<Value>,
}

impl Query {
    pub fn parse<S: AsRef<str>>(terms: &[S]) -> Result<Self> {
        let mut query = Query::default();
        for term in terms {
            query.add(term.as_ref())?;
        }
        Ok(query)
    }

    fn add(&mut self, term: &str) -> Result<()> {
        let (negated, term) = match term.strip_prefix('^') {
            Some(rest) => (true, rest),
            None => (false, term),
        };
        let clause = match term.split_once(':') {
            Some((name, value)) if field(name).is_some() => {
                let field = field(name).unwrap();
                let condition = self.condition(name, field, value)?;
                match field.table {
                    Some(table) => format!(
                        "EXISTS (SELECT 1 FROM {table} s WHERE s.item_id = items.id AND {condition})"
                    ),
                    None => condition,
                }
            }
            _ => {
                self.params.push(Value::Text(term.to_string()));
                let n = self.params.len();
                format!(
                    "(instr(lower(title), lower(?{n})) > 0 OR instr(lower(coalesce(episode_title, '')), lower(?{n})) > 0)"
                )
            }
        };
        self.clauses.push(if negated {
            format!("NOT coalesce({clause}, 0)")
        } else {
            clause
        });
        Ok(())
    }

    fn bind(&mut self, value: Value) -> String {
        self.params.push(value);
        format!("?{}", self.params.len())
    }

    fn condition(&mut self, name: &str, field: Field, value: &str) -> Result<String> {
        let column = match field.table {
            Some(_) => format!("s.{}", field.column),
            None => format!("items.{}", field.column),
        };
        Ok(match field.kind {
            FieldKind::Text => match value.strip_prefix('=') {
                Some(exact) => {
                    let p = self.bind(Value::Text(exact.to_string()));
                    format!("lower(coalesce({column}, '')) = lower({p})")
                }
                None => {
                    let p = self.bind(Value::Text(value.to_string()));
                    format!("instr(lower(coalesce({column}, '')), lower({p})) > 0")
                }
            },
            FieldKind::List => {
                let p = self.bind(Value::Text(value.to_string()));
                format!(
                    "instr('/' || lower(coalesce({column}, '')) || '/', '/' || lower({p}) || '/') > 0"
                )
            }
            FieldKind::Flag => {
                let wanted = match value.to_lowercase().as_str() {
                    "true" | "yes" | "1" => 1,
                    "false" | "no" | "0" => 0,
                    _ => bail!("{name}: expected true or false, got {value:?}"),
                };
                // A false flag is only meaningful per stream, so `atmos:false` finds files with
                // a non-Atmos stream; use `^atmos:true` for files with no Atmos at all.
                format!("{column} = {wanted}")
            }
            FieldKind::Number => {
                let number = |text: &str| -> Result<Value> {
                    text.parse::<f64>()
                        .map(Value::Real)
                        .map_err(|_| anyhow::anyhow!("{name}: {text:?} is not a number"))
                };
                match value.split_once("..") {
                    None => {
                        let p = self.bind(number(value)?);
                        format!("{column} = {p}")
                    }
                    Some((low, high)) => {
                        let mut bounds = Vec::new();
                        if !low.is_empty() {
                            let p = self.bind(number(low)?);
                            bounds.push(format!("{column} >= {p}"));
                        }
                        if !high.is_empty() {
                            let p = self.bind(number(high)?);
                            bounds.push(format!("{column} <= {p}"));
                        }
                        if bounds.is_empty() {
                            format!("{column} IS NOT NULL")
                        } else {
                            format!("({})", bounds.join(" AND "))
                        }
                    }
                }
            }
        })
    }

    pub fn where_clause(&self) -> String {
        if self.clauses.is_empty() {
            "1".to_string()
        } else {
            self.clauses.join(" AND ")
        }
    }

    pub fn params(&self) -> &[Value] {
        &self.params
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::Library;
    use crate::model::{AudioStream, Item, Kind, SubtitleStream};

    fn library() -> Library {
        let mut matrix = Item::new(Kind::Movie, "/m/matrix.mkv".into(), "The Matrix".into());
        matrix.year = Some(1999);
        matrix.edition = Some("Final Cut".into());
        matrix.size = 5_000;
        matrix.media.resolution = Some("2160p".into());
        matrix.media.width = Some(3840);
        matrix.media.hdr = Some("DV/HDR10+/HDR10".into());
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
                language: Some("eng".into()),
                ..AudioStream::default()
            },
        ];
        matrix.media.subtitles = vec![SubtitleStream {
            language: Some("eng".into()),
            forced: true,
            ..SubtitleStream::default()
        }];

        let mut heat = Item::new(Kind::Movie, "/m/heat.mkv".into(), "Heat".into());
        heat.year = Some(1995);
        heat.size = 3_000;
        heat.media.resolution = Some("1080p".into());
        heat.media.hdr = Some("HDR10".into());
        heat.media.audio = vec![AudioStream {
            codec: Some("dts".into()),
            channels: Some(6),
            ..AudioStream::default()
        }];

        let mut rose = Item::new(Kind::Episode, "/tv/rose.mkv".into(), "Doctor Who".into());
        rose.year = Some(2005);
        rose.season = Some(1);
        rose.episode = Some(1);
        rose.episode_title = Some("Rose".into());
        rose.media.resolution = Some("1080p".into());
        rose.media.hdr = Some("SDR".into());

        let untitled = Item::new(Kind::Movie, "/m/untitled.mkv".into(), "Untitled".into());

        let mut library = Library::in_memory().unwrap();
        for mut item in [matrix, heat, rose, untitled] {
            library.save(&mut item).unwrap();
        }
        library
    }

    fn matching(terms: &[&str]) -> Vec<String> {
        let query = Query::parse(terms).unwrap();
        let mut titles: Vec<_> = library()
            .query(&query)
            .unwrap()
            .into_iter()
            .map(|item| item.title)
            .collect();
        titles.sort();
        titles
    }

    fn error(terms: &[&str]) -> String {
        Query::parse(terms).unwrap_err().to_string()
    }

    #[test]
    fn empty_query_matches_everything() {
        assert_eq!(matching(&[]).len(), 4);
        assert_eq!(Query::default().where_clause(), "1");
        assert!(Query::default().params().is_empty());
    }

    #[test]
    fn bare_words_match_titles_and_episode_titles_ignoring_case() {
        assert_eq!(matching(&["matrix"]), ["The Matrix"]);
        assert_eq!(matching(&["HEAT"]), ["Heat"]);
        assert_eq!(matching(&["rose"]), ["Doctor Who"]);
        assert_eq!(matching(&["who", "rose"]), ["Doctor Who"]);
        assert!(matching(&["nonexistent"]).is_empty());
    }

    #[test]
    fn terms_are_anded() {
        assert_eq!(matching(&["kind:movie", "year:1990..1999"]).len(), 2);
        assert_eq!(matching(&["kind:movie", "heat"]), ["Heat"]);
        assert!(matching(&["heat", "matrix"]).is_empty());
    }

    #[test]
    fn unknown_fields_fall_back_to_a_title_search() {
        assert!(matching(&["nosuchfield:value"]).is_empty());
        let query = Query::parse(&["nosuchfield:value"]).unwrap();
        assert_eq!(query.params(), [Value::Text("nosuchfield:value".into())]);
    }

    #[test]
    fn text_fields_match_substrings_or_exactly() {
        assert_eq!(matching(&["title:matr"]), ["The Matrix"]);
        assert_eq!(matching(&["title:=heat"]), ["Heat"]);
        assert!(matching(&["title:=hea"]).is_empty());
        assert_eq!(matching(&["edition:final"]), ["The Matrix"]);
        assert_eq!(matching(&["resolution:=1080p"]), ["Doctor Who", "Heat"]);
    }

    #[test]
    fn negation_excludes_matches_and_keeps_nulls() {
        assert_eq!(matching(&["^kind:movie"]), ["Doctor Who"]);
        assert_eq!(matching(&["^matrix", "kind:movie"]), ["Heat", "Untitled"]);
        assert_eq!(
            matching(&["kind:movie", "^year:1999"]),
            ["Heat", "Untitled"],
            "a missing year is not 1999"
        );
        assert_eq!(matching(&["^edition:final"]).len(), 3);
    }

    #[test]
    fn numbers_match_exactly_and_in_ranges() {
        assert_eq!(matching(&["year:1999"]), ["The Matrix"]);
        assert_eq!(matching(&["year:1990..1999"]), ["Heat", "The Matrix"]);
        assert_eq!(matching(&["year:2000.."]), ["Doctor Who"]);
        assert_eq!(matching(&["year:..1995"]), ["Heat"]);
        assert_eq!(matching(&["year:.."]).len(), 3, "any known year");
        assert_eq!(matching(&["size:3000..5000"]), ["Heat", "The Matrix"]);
        assert_eq!(matching(&["season:1", "episode:1"]), ["Doctor Who"]);
        assert_eq!(matching(&["width:3840"]), ["The Matrix"]);
    }

    #[test]
    fn numbers_must_parse() {
        assert!(error(&["year:abc"]).contains("not a number"));
        assert!(error(&["year:1990..abc"]).contains("not a number"));
        assert!(error(&["year:abc.."]).contains("year"));
    }

    #[test]
    fn list_fields_match_whole_elements() {
        assert_eq!(matching(&["hdr:hdr10"]), ["Heat", "The Matrix"]);
        assert_eq!(matching(&["hdr:hdr10+"]), ["The Matrix"]);
        assert_eq!(matching(&["hdr:dv"]), ["The Matrix"]);
        assert_eq!(matching(&["hdr:sdr"]), ["Doctor Who"]);
        assert!(matching(&["hdr:hdr"]).is_empty());
        assert_eq!(matching(&["hdr:hdr10", "^hdr:hdr10+"]), ["Heat"]);
    }

    #[test]
    fn stream_fields_match_if_any_stream_matches() {
        assert_eq!(matching(&["audio_codec:truehd"]), ["The Matrix"]);
        assert_eq!(matching(&["audio_codec:ac3"]), ["The Matrix"]);
        assert_eq!(matching(&["audio_channels:6"]), ["Heat", "The Matrix"]);
        assert_eq!(matching(&["audio_channels:7.."]), ["The Matrix"]);
        assert_eq!(matching(&["audio_language:eng"]), ["The Matrix"]);
        assert_eq!(matching(&["subtitle_language:eng"]), ["The Matrix"]);
    }

    #[test]
    fn stream_terms_may_match_different_streams() {
        assert_eq!(
            matching(&["audio_codec:truehd", "audio_channels:6"]),
            ["The Matrix"]
        );
    }

    #[test]
    fn flags_accept_several_spellings() {
        for spelling in ["true", "TRUE", "yes", "1"] {
            assert_eq!(matching(&[&format!("atmos:{spelling}")]), ["The Matrix"]);
        }
        for spelling in ["false", "no", "0"] {
            assert_eq!(
                matching(&[&format!("atmos:{spelling}")]),
                ["Heat", "The Matrix"],
                "a false flag finds files with a stream that lacks it"
            );
        }
        assert_eq!(matching(&["subtitle_forced:true"]), ["The Matrix"]);
        assert!(error(&["atmos:maybe"]).contains("expected true or false"));
    }

    #[test]
    fn negated_flags_find_files_without_any() {
        assert_eq!(
            matching(&["kind:movie", "^atmos:true"]),
            ["Heat", "Untitled"]
        );
        assert_eq!(matching(&["^subtitle_forced:true", "kind:movie"]).len(), 2);
    }

    #[test]
    fn terms_with_colons_in_values_keep_them() {
        assert!(matching(&["title:a:b"]).is_empty());
    }

    #[test]
    fn every_query_field_is_documented_by_fields() {
        for name in Item::FIELDS {
            let queryable = field(name).is_some();
            assert!(
                queryable || *name == "original_path",
                "{name} cannot be queried"
            );
        }
    }
}
