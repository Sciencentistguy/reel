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
