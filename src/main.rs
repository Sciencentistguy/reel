use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};

mod commands;
mod config;
mod fileops;
mod import;
mod library;
mod metadata;
mod missing;
mod model;
mod naming;
mod parse;
mod probe;
mod query;
mod sync;

use config::Config;
use library::Library;
use model::Item;
use query::Query;

/// Organise a library of films and TV shows using Plex's naming conventions.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Configuration file to use instead of the default.
    #[arg(short, long, global = true, value_name = "FILE", env = "REEL_CONFIG")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Identify video files and file them into the library.
    Import(import::Options),
    /// Add files already in the library folders but missing from the database.
    Rescan(import::Rescan),
    /// List library items matching a query.
    #[command(visible_alias = "ls")]
    List {
        /// Output template using $field references, e.g. '$title ($year) $hdr'.
        #[arg(short, long, conflicts_with = "path")]
        format: Option<String>,
        /// Print file paths.
        #[arg(short, long)]
        path: bool,
        query: Vec<String>,
    },
    /// Show everything known about matching items, including their streams.
    Info {
        #[arg(required = true)]
        query: Vec<String>,
    },
    /// Rename matching files to follow the current naming scheme.
    #[command(visible_alias = "mv")]
    Move {
        #[arg(short = 'n', long, visible_alias = "dry-run")]
        pretend: bool,
        query: Vec<String>,
    },
    /// Refresh identified items from their metadata source and rename files to match.
    Sync(sync::Options),
    /// List aired episodes of shows that have no file in the library.
    Missing(missing::Options),
    /// Re-probe changed files and drop entries whose files have gone.
    Update {
        #[arg(short = 'n', long, visible_alias = "dry-run")]
        pretend: bool,
        /// Re-probe every matching file, changed or not.
        #[arg(short, long)]
        force: bool,
        /// Remove entries for missing files without asking.
        #[arg(short, long)]
        yes: bool,
        query: Vec<String>,
    },
    /// Remove matching items from the library.
    #[command(visible_alias = "rm")]
    Remove {
        /// Also delete the files (and their sidecar subtitles) from disk.
        #[arg(short, long)]
        delete: bool,
        /// Do not ask for confirmation.
        #[arg(short, long)]
        yes: bool,
        #[arg(required = true)]
        query: Vec<String>,
    },
    /// Remove items lacking the metadata needed to identify and name them.
    Purge {
        /// Also delete the files (and their sidecar subtitles) from disk.
        #[arg(short, long)]
        delete: bool,
        /// Do not ask for confirmation.
        #[arg(short, long)]
        yes: bool,
        query: Vec<String>,
    },
    /// Summarise the library, or the part of it matching a query.
    Stats { query: Vec<String> },
    /// List the fields usable in queries and format templates.
    Fields,
    /// Show the effective configuration.
    Config {
        /// Print the configuration file's location.
        #[arg(long)]
        path: bool,
        /// Write a configuration file populated with the defaults.
        #[arg(long, conflicts_with = "path")]
        init: bool,
    },
}

fn run(cli: Cli) -> Result<()> {
    let initialising = matches!(cli.command, Command::Config { init: true, .. });
    let config_path = cli.config.unwrap_or_else(|| {
        if initialising {
            config::user_path()
        } else {
            config::default_path()
        }
    });
    let config = Config::load(&config_path)?;
    let open_library = || Library::open(&config.library.database);

    match cli.command {
        Command::Import(options) => import::run(&config, &mut open_library()?, &options),
        Command::Rescan(options) => import::rescan(&config, &mut open_library()?, options),
        Command::List {
            format,
            path,
            query,
        } => commands::list(
            &open_library()?,
            &Query::parse(&query)?,
            format.as_deref(),
            path,
        ),
        Command::Info { query } => commands::info(&open_library()?, &Query::parse(&query)?),
        Command::Move { pretend, query } => commands::move_items(
            &config,
            &mut open_library()?,
            &Query::parse(&query)?,
            pretend,
        ),
        Command::Sync(options) => sync::run(&config, &mut open_library()?, &options),
        Command::Missing(options) => missing::run(&config, &open_library()?, &options),
        Command::Update {
            pretend,
            force,
            yes,
            query,
        } => commands::update(
            &mut open_library()?,
            &Query::parse(&query)?,
            pretend,
            force,
            yes,
        ),
        Command::Remove { delete, yes, query } => commands::remove(
            &config,
            &mut open_library()?,
            &Query::parse(&query)?,
            delete,
            yes,
        ),
        Command::Purge { delete, yes, query } => commands::purge(
            &config,
            &mut open_library()?,
            &Query::parse(&query)?,
            delete,
            yes,
        ),
        Command::Stats { query } => commands::stats(&open_library()?, &Query::parse(&query)?),
        Command::Fields => {
            for field in Item::FIELDS {
                println!("{field}");
            }
            Ok(())
        }
        Command::Config { path, init } => commands::config(&config, &config_path, path, init),
    }
}

/// Exit quietly, as other command-line tools do, when output is piped into something like `head`.
#[cfg(unix)]
fn restore_sigpipe() {
    // SAFETY: called before any other thread exists, and SIG_DFL is always a valid disposition.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
fn restore_sigpipe() {}

fn main() -> ExitCode {
    restore_sigpipe();
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::model::Kind;
    use clap::CommandFactory;

    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("reel").chain(args.iter().copied()))
    }

    #[test]
    fn the_command_line_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn subcommand_aliases_work() {
        assert!(matches!(
            parse(&["ls"]).unwrap().command,
            Command::List { .. }
        ));
        assert!(matches!(
            parse(&["mv"]).unwrap().command,
            Command::Move { .. }
        ));
        assert!(matches!(
            parse(&["rm", "heat"]).unwrap().command,
            Command::Remove { .. }
        ));
    }

    #[test]
    fn dry_run_is_an_alias_for_pretend() {
        for args in [
            ["import", "--dry-run", "x"],
            ["import", "--pretend", "x"],
            ["import", "-n", "x"],
        ] {
            match parse(&args).unwrap().command {
                Command::Import(options) => assert!(options.pretend, "{args:?}"),
                _ => panic!("expected import"),
            }
        }
        for command in ["move", "update", "rescan"] {
            assert!(parse(&[command, "--dry-run"]).is_ok(), "{command}");
        }
    }

    #[test]
    fn kinds_accept_tv_as_an_alias() {
        for (name, kind) in [
            ("movie", Kind::Movie),
            ("episode", Kind::Episode),
            ("tv", Kind::Episode),
        ] {
            match parse(&["import", "--kind", name, "x"]).unwrap().command {
                Command::Import(options) => assert_eq!(options.kind, Some(kind)),
                _ => panic!("expected import"),
            }
        }
        assert!(parse(&["import", "--kind", "film", "x"]).is_err());
    }

    #[test]
    fn required_arguments_are_enforced() {
        assert!(parse(&[]).is_err());
        assert!(parse(&["import"]).is_err());
        assert!(parse(&["info"]).is_err());
        assert!(parse(&["remove"]).is_err());
        assert!(parse(&["list"]).is_ok());
        assert!(parse(&["stats"]).is_ok());
        assert!(parse(&["rescan"]).is_ok());
        assert!(parse(&["sync", "-nM", "kind:episode"]).is_ok());
        assert!(parse(&["missing", "--specials", "title:lost"]).is_ok());
    }

    #[test]
    fn contradictory_options_are_rejected() {
        assert!(parse(&["import", "--timid", "--no-timid", "x"]).is_err());
        assert!(parse(&["rescan", "--timid", "--no-timid"]).is_err());
        assert!(parse(&["list", "--format", "$title", "--path"]).is_err());
        assert!(parse(&["config", "--init", "--path"]).is_err());
        assert!(parse(&["import", "--operation", "teleport", "x"]).is_err());
    }

    #[test]
    fn the_config_option_is_accepted_anywhere() {
        let before = parse(&["-c", "a.toml", "stats"]).unwrap();
        let after = parse(&["stats", "-c", "a.toml"]).unwrap();
        assert_eq!(before.config, Some(PathBuf::from("a.toml")));
        assert_eq!(after.config, before.config);
    }

    #[test]
    fn queries_are_collected_as_separate_terms() {
        match parse(&["list", "kind:movie", "^hdr:dv", "heat"])
            .unwrap()
            .command
        {
            Command::List { query, .. } => assert_eq!(query, ["kind:movie", "^hdr:dv", "heat"]),
            _ => panic!("expected list"),
        }
    }

    /// Runs the real commands through `run`, as the binary does, against a throwaway library.
    #[test]
    fn commands_work_together_through_the_command_line() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            "[library]\ndatabase = \"library.db\"\nmovies = \"Movies\"\ntv = \"TV\"\n\
             [import]\nprobe = false\n",
        )
        .unwrap();
        let film = dir
            .path()
            .join("Movies/Heat (1995) {tmdb-949}/Heat (1995) {tmdb-949}.mkv");
        std::fs::create_dir_all(film.parent().unwrap()).unwrap();
        std::fs::write(&film, "video").unwrap();
        {
            let mut library = Library::open(&dir.path().join("library.db")).unwrap();
            let mut item = Item::new(Kind::Movie, film.clone(), "Heat".into());
            item.year = Some(1995);
            (item.source, item.source_id) = (Some("tmdb".into()), Some("949".into()));
            (item.size, item.mtime) = import::file_stamp(&film).unwrap();
            library.save(&mut item).unwrap();
        }

        let reel = |args: &[&str]| {
            let config = config.to_str().unwrap();
            let mut full = vec!["-c", config];
            full.extend_from_slice(args);
            run(parse(&full).unwrap())
        };
        let count = |terms: &[&str]| {
            let library = Library::open(&dir.path().join("library.db")).unwrap();
            library.query(&Query::parse(terms).unwrap()).unwrap().len()
        };

        assert_eq!(count(&[]), 1);

        reel(&["list"]).unwrap();
        reel(&["list", "--path", "heat"]).unwrap();
        reel(&["info", "heat"]).unwrap();
        reel(&["stats"]).unwrap();
        reel(&["fields"]).unwrap();
        reel(&["config"]).unwrap();
        reel(&["move", "--pretend"]).unwrap();
        reel(&["update", "--yes"]).unwrap();
        reel(&["missing"]).unwrap();
        reel(&["purge", "--yes"]).unwrap();
        assert_eq!(count(&[]), 1);

        reel(&["remove", "--yes", "--delete", "heat"]).unwrap();
        assert_eq!(count(&[]), 0);
        assert!(!film.parent().unwrap().exists());

        assert!(
            reel(&["list", "year:abc"]).is_err(),
            "bad queries are reported"
        );
    }
}
