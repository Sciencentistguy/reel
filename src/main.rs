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
mod model;
mod naming;
mod parse;
mod probe;
mod query;

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
        #[arg(short = 'n', long)]
        pretend: bool,
        query: Vec<String>,
    },
    /// Re-probe changed files and drop entries whose files have gone.
    Update {
        #[arg(short = 'n', long)]
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
    let config_path = cli.config.unwrap_or_else(config::default_path);
    let config = Config::load(&config_path)?;
    let open_library = || Library::open(&config.library.database);

    match cli.command {
        Command::Import(options) => import::run(&config, &mut open_library()?, &options),
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
