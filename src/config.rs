use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub library: LibraryConfig,
    pub import: ImportConfig,
    pub naming: NamingConfig,
    pub metadata: MetadataConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LibraryConfig {
    pub database: PathBuf,
    pub movies: PathBuf,
    pub tv: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Operation {
    Copy,
    Move,
    Hardlink,
    Symlink,
    Reflink,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ImportConfig {
    pub operation: Operation,
    /// Bring subtitle files that sit next to a video along with it.
    pub subtitles: bool,
    /// Run ffprobe on imported files.
    pub probe: bool,
    /// Matches at or below this distance (0 = identical) are applied without asking.
    pub strong_match: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NamingConfig {
    /// Append `{tmdb-123}` / `{tvdb-123}` to film and show folders.
    pub id_tags: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TvSource {
    Tvdb,
    Tmdb,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetadataConfig {
    pub tv_source: TvSource,
    /// TMDB v3 API key or v4 read access token. `TMDB_API_KEY` overrides it.
    pub tmdb_api_key: String,
    /// TVDB v4 API key. `TVDB_API_KEY` overrides it.
    pub tvdb_api_key: String,
    /// TMDB language tag.
    pub tmdb_language: String,
    /// TVDB three-letter language code.
    pub tvdb_language: String,
}

impl Default for LibraryConfig {
    fn default() -> Self {
        let data = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
        let videos = dirs::video_dir()
            .or_else(|| dirs::home_dir().map(|home| home.join("Videos")))
            .unwrap_or_else(|| PathBuf::from("."));
        LibraryConfig {
            database: data.join("vlib").join("library.db"),
            movies: videos.join("Movies"),
            tv: videos.join("TV Shows"),
        }
    }
}

impl Default for ImportConfig {
    fn default() -> Self {
        ImportConfig {
            operation: Operation::Copy,
            subtitles: true,
            probe: true,
            strong_match: 0.08,
        }
    }
}

impl Default for NamingConfig {
    fn default() -> Self {
        NamingConfig { id_tags: true }
    }
}

impl Default for MetadataConfig {
    fn default() -> Self {
        MetadataConfig {
            tv_source: TvSource::Tvdb,
            tmdb_api_key: String::new(),
            tvdb_api_key: String::new(),
            tmdb_language: "en-US".into(),
            tvdb_language: "eng".into(),
        }
    }
}

pub fn default_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("vlib")
        .join("config.toml")
}

fn expand_tilde(path: &Path) -> PathBuf {
    match (path.strip_prefix("~"), dirs::home_dir()) {
        (Ok(rest), Some(home)) => home.join(rest),
        _ => path.to_path_buf(),
    }
}

impl Config {
    /// Loads the config file, falling back to defaults when it does not exist.
    pub fn load(path: &Path) -> Result<Self> {
        let mut config: Config = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .with_context(|| format!("invalid config file {}", path.display()))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(err) => {
                return Err(err).with_context(|| format!("cannot read {}", path.display()));
            }
        };
        for dir in [
            &mut config.library.database,
            &mut config.library.movies,
            &mut config.library.tv,
        ] {
            *dir = expand_tilde(dir);
        }
        for (var, key) in [
            ("TMDB_API_KEY", &mut config.metadata.tmdb_api_key),
            ("TVDB_API_KEY", &mut config.metadata.tvdb_api_key),
        ] {
            if let Ok(value) = std::env::var(var)
                && !value.is_empty()
            {
                *key = value;
            }
        }
        Ok(config)
    }

    pub fn to_toml(&self) -> Result<String> {
        Ok(toml::to_string_pretty(self)?)
    }

    pub fn redacted(&self) -> Self {
        let mut config = self.clone();
        for key in [
            &mut config.metadata.tmdb_api_key,
            &mut config.metadata.tvdb_api_key,
        ] {
            if !key.is_empty() {
                *key = "<redacted>".into();
            }
        }
        config
    }
}
