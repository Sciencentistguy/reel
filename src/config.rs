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
    /// Remux video files into Matroska with videoconverter, leaving the source in place.
    Remux,
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
    /// Always ask which match to use, even when one is certain.
    pub timid: bool,
    /// The videoconverter program used by the `remux` operation.
    pub videoconverter: String,
    /// Extra videoconverter arguments for the `remux` operation, such as `--anime`.
    pub remux_args: Vec<String>,
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
    /// File holding the TMDB key, for keeping secrets out of the configuration file. Used when
    /// neither `tmdb_api_key` nor `TMDB_API_KEY` is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tmdb_api_key_file: Option<PathBuf>,
    /// File holding the TVDB key, used like `tmdb_api_key_file`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tvdb_api_key_file: Option<PathBuf>,
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
            database: data.join("reel").join("library.db"),
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
            timid: false,
            videoconverter: "videoconverter".into(),
            remux_args: Vec::new(),
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
            tmdb_api_key_file: None,
            tvdb_api_key_file: None,
            tmdb_language: "en-US".into(),
            tvdb_language: "eng".into(),
        }
    }
}

const SYSTEM_PATH: &str = "/etc/reel/config.toml";

/// The per-user configuration file, which is the only one reel ever writes.
pub fn user_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("reel")
        .join("config.toml")
}

/// The user's configuration file, or the system-wide one when the user has none.
pub fn default_path() -> PathBuf {
    let user = user_path();
    let system = Path::new(SYSTEM_PATH);
    if !user.exists() && system.exists() {
        system.to_path_buf()
    } else {
        user
    }
}

/// Expands a leading `~` and resolves relative paths against the config file's directory.
fn resolve(path: &Path, config_dir: &Path) -> PathBuf {
    match (path.strip_prefix("~"), dirs::home_dir()) {
        (Ok(rest), Some(home)) => home.join(rest),
        _ => config_dir.join(path),
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
        let config_dir = std::path::absolute(path)?;
        let config_dir = config_dir.parent().unwrap_or(Path::new("/"));
        for dir in [
            &mut config.library.database,
            &mut config.library.movies,
            &mut config.library.tv,
        ] {
            *dir = resolve(dir, config_dir);
        }
        let metadata = &mut config.metadata;
        for (var, key, file) in [
            (
                "TMDB_API_KEY",
                &mut metadata.tmdb_api_key,
                &metadata.tmdb_api_key_file,
            ),
            (
                "TVDB_API_KEY",
                &mut metadata.tvdb_api_key,
                &metadata.tvdb_api_key_file,
            ),
        ] {
            if let Ok(value) = std::env::var(var)
                && !value.is_empty()
            {
                *key = value;
            } else if key.is_empty()
                && let Some(file) = file
            {
                let file = resolve(file, config_dir);
                // Only imports need the key, so an unreadable file must not break other commands.
                match std::fs::read_to_string(&file) {
                    Ok(contents) => *key = contents.trim().to_string(),
                    Err(err) => eprintln!("warning: cannot read {}: {err}", file.display()),
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_and_key_files_resolve_against_the_config_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("tvdb.key"), "secret\n").unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[library]\nmovies = \"Movies\"\ntv = \"/abs/TV\"\n\
             [metadata]\ntvdb_api_key_file = \"tvdb.key\"\n",
        )
        .unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.library.movies, dir.path().join("Movies"));
        assert_eq!(config.library.tv, Path::new("/abs/TV"));
        if std::env::var_os("TVDB_API_KEY").is_none() {
            assert_eq!(config.metadata.tvdb_api_key, "secret");
        }
        assert!(config.redacted().to_toml().unwrap().contains("<redacted>"));
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[library]\nmovie = \"x\"\n").unwrap();
        assert!(Config::load(&path).is_err());
    }

    fn load(toml: &str) -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, toml).unwrap();
        let config = Config::load(&path).unwrap();
        (dir, config)
    }

    #[test]
    fn a_missing_file_gives_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load(&dir.path().join("absent.toml")).unwrap();
        assert_eq!(config.import.operation, Operation::Copy);
        assert!(config.import.subtitles && config.import.probe && !config.import.timid);
        assert!(config.naming.id_tags);
        assert_eq!(config.metadata.tv_source, TvSource::Tvdb);
        assert_eq!(config.import.strong_match, 0.08);
    }

    #[test]
    fn the_default_configuration_loads_back_unchanged() {
        let defaults = Config::default();
        let (_dir, loaded) = load(&defaults.to_toml().unwrap());
        assert_eq!(loaded.library.database, defaults.library.database);
        assert_eq!(loaded.library.movies, defaults.library.movies);
        assert_eq!(loaded.import.operation, defaults.import.operation);
        assert_eq!(loaded.metadata.tmdb_language, "en-US");
        assert_eq!(loaded.metadata.tvdb_language, "eng");
    }

    #[test]
    fn partial_files_fill_in_the_rest_with_defaults() {
        let (_dir, config) = load("[import]\noperation = \"hardlink\"\n");
        assert_eq!(config.import.operation, Operation::Hardlink);
        assert!(config.import.subtitles);
        assert_eq!(config.metadata.tv_source, TvSource::Tvdb);
    }

    #[test]
    fn enumerations_parse_from_lowercase_names() {
        for (name, operation) in [
            ("copy", Operation::Copy),
            ("move", Operation::Move),
            ("hardlink", Operation::Hardlink),
            ("symlink", Operation::Symlink),
            ("reflink", Operation::Reflink),
            ("remux", Operation::Remux),
        ] {
            let (_dir, config) = load(&format!("[import]\noperation = \"{name}\"\n"));
            assert_eq!(config.import.operation, operation);
        }
        let (_dir, config) = load("[metadata]\ntv_source = \"tmdb\"\n");
        assert_eq!(config.metadata.tv_source, TvSource::Tmdb);
    }

    #[test]
    fn invalid_values_and_syntax_are_rejected() {
        for bad in [
            "[import]\noperation = \"teleport\"\n",
            "[metadata]\ntv_source = \"imdb\"\n",
            "[import]\nstrong_match = \"high\"\n",
            "[nonsense]\n",
            "this is not toml",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("config.toml");
            std::fs::write(&path, bad).unwrap();
            let error = Config::load(&path).unwrap_err();
            assert!(
                format!("{error:#}").contains("invalid config file"),
                "{bad}"
            );
        }
    }

    #[test]
    fn tilde_expands_to_the_home_directory() {
        let Some(home) = dirs::home_dir() else { return };
        let (_dir, config) = load("[library]\nmovies = \"~/Films\"\n");
        assert_eq!(config.library.movies, home.join("Films"));
    }

    #[test]
    fn relative_library_paths_follow_the_config_file() {
        let (dir, config) = load("[library]\ndatabase = \"db/library.db\"\ntv = \"Shows\"\n");
        assert_eq!(config.library.database, dir.path().join("db/library.db"));
        assert_eq!(config.library.tv, dir.path().join("Shows"));
    }

    #[test]
    fn inline_keys_are_kept_unless_the_environment_overrides_them() {
        let (_dir, config) = load("[metadata]\ntmdb_api_key = \"inline\"\n");
        if std::env::var_os("TMDB_API_KEY").is_none() {
            assert_eq!(config.metadata.tmdb_api_key, "inline");
        }
    }

    #[test]
    fn an_inline_key_wins_over_a_key_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("tvdb.key"), "from-file\n").unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[metadata]\ntvdb_api_key = \"inline\"\ntvdb_api_key_file = \"tvdb.key\"\n",
        )
        .unwrap();
        let config = Config::load(&path).unwrap();
        if std::env::var_os("TVDB_API_KEY").is_none() {
            assert_eq!(config.metadata.tvdb_api_key, "inline");
        }
    }

    #[test]
    fn an_unreadable_key_file_does_not_break_loading() {
        let (_dir, config) = load("[metadata]\ntmdb_api_key_file = \"missing.key\"\n");
        if std::env::var_os("TMDB_API_KEY").is_none() {
            assert!(config.metadata.tmdb_api_key.is_empty());
        }
    }

    #[test]
    fn redaction_hides_set_keys_without_changing_the_original() {
        let (_dir, config) = load("[metadata]\ntmdb_api_key = \"secret\"\n");
        let redacted = config.redacted();
        let text = redacted.to_toml().unwrap();
        assert!(!text.contains("secret"));
        assert!(text.contains("tmdb_api_key = \"<redacted>\""));
        assert_ne!(redacted.metadata.tmdb_api_key, "");
        assert_ne!(config.metadata.tmdb_api_key, "<redacted>");
        assert_eq!(
            redacted.metadata.tvdb_api_key.is_empty(),
            config.metadata.tvdb_api_key.is_empty()
        );
    }
}
