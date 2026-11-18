//! Library maintenance commands: list, info, move, update, remove, purge and stats.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::config::{Config, Operation};
use crate::fileops;
use crate::import::{file_stamp, prompt};
use crate::library::Library;
use crate::model::{Item, Kind};
use crate::naming;
use crate::probe;
use crate::query::Query;

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    match unit {
        0 => format!("{bytes} B"),
        _ => format!("{value:.1} {}", UNITS[unit]),
    }
}

pub fn human_duration(seconds: f64) -> String {
    let total = seconds.round() as u64;
    let (days, hours, minutes) = (total / 86_400, total % 86_400 / 3600, total % 3600 / 60);
    match days {
        0 => format!("{hours}h {minutes:02}m"),
        _ => format!("{days}d {hours}h {minutes:02}m"),
    }
}

pub fn list(library: &Library, query: &Query, format: Option<&str>, paths: bool) -> Result<()> {
    for item in library.query(query)? {
        match format {
            Some(template) => println!("{}", item.format(template)),
            None if paths => println!("{}", item.path.display()),
            None => println!("{item}"),
        }
    }
    Ok(())
}

pub fn info(library: &Library, query: &Query) -> Result<()> {
    let join =
        |parts: Vec<Option<String>>| parts.into_iter().flatten().collect::<Vec<_>>().join(", ");
    for item in library.query(query)? {
        let media = &item.media;
        println!("{item}");
        println!("  path:      {}", item.path.display());
        if let Some(original) = &item.original_path {
            println!("  original:  {}", original.display());
        }
        if let (Some(source), Some(id)) = (&item.source, &item.source_id) {
            println!("  metadata:  {source}-{id}");
        }
        println!("  size:      {}", human_size(item.size));
        if let Some(duration) = media.duration {
            println!("  duration:  {}", human_duration(duration));
        }
        let video = join(vec![
            media.video_codec.clone(),
            media
                .width
                .zip(media.height)
                .map(|(w, h)| format!("{w}x{h}")),
            media.bit_depth.map(|bits| format!("{bits}-bit")),
            media.hdr.clone(),
            media
                .dv_profile
                .as_ref()
                .map(|profile| format!("DV profile {profile}")),
        ]);
        if !video.is_empty() {
            println!("  video:     {video}");
        }
        let colour = join(vec![
            media.colour_space.as_ref().map(|v| format!("space {v}")),
            media
                .colour_primaries
                .as_ref()
                .map(|v| format!("primaries {v}")),
            media.transfer.as_ref().map(|v| format!("transfer {v}")),
        ]);
        if !colour.is_empty() {
            println!("  colour:    {colour}");
        }
        for (number, stream) in media.audio.iter().enumerate() {
            let description = join(vec![
                stream.language.clone(),
                stream.codec.clone(),
                stream.profile.clone(),
                stream
                    .layout
                    .clone()
                    .or(stream.channels.map(|c| format!("{c} channels"))),
                stream.atmos.then(|| "Atmos".to_string()),
                stream.title.as_ref().map(|title| format!("{title:?}")),
            ]);
            println!("  audio {:<4} {description}", format!("{}:", number + 1));
        }
        for (number, stream) in media.subtitles.iter().enumerate() {
            let description = join(vec![
                stream.language.clone(),
                stream.codec.clone(),
                stream.forced.then(|| "forced".to_string()),
                stream.sdh.then(|| "SDH".to_string()),
                stream.title.as_ref().map(|title| format!("{title:?}")),
            ]);
            println!("  subs {:<5} {description}", format!("{}:", number + 1));
        }
        for (sidecar, _) in naming::sidecars(&item.path) {
            let name = sidecar.file_name().unwrap_or_default().to_string_lossy();
            println!("  sidecar:   {name}");
        }
        println!();
    }
    Ok(())
}

/// Renames files to match the current metadata and naming configuration.
pub fn move_items(
    config: &Config,
    library: &mut Library,
    query: &Query,
    pretend: bool,
) -> Result<()> {
    let mut moved = 0;
    let mut claimed = HashSet::new();
    for mut item in library.query(query)? {
        if relocate(config, library, &mut item, &mut claimed, pretend)? {
            if !pretend {
                library.save(&mut item)?;
            }
            moved += 1;
        }
    }
    let verb = if pretend { "Would move" } else { "Moved" };
    println!("{verb} {moved} file(s).");
    Ok(())
}

/// Moves an item's file and sidecars to where its metadata says it belongs, updating its path
/// but not saving it. Returns whether it moved, or would have when pretending.
pub fn relocate(
    config: &Config,
    library: &Library,
    item: &mut Item,
    claimed: &mut HashSet<PathBuf>,
    pretend: bool,
) -> Result<bool> {
    let versioned = library.has_other_versions(item)?;
    let destination = naming::free_destination(config, item, versioned, &item.path, |path| {
        claimed.contains(path) || path.symlink_metadata().is_ok()
    });
    claimed.insert(destination.clone());
    if destination == item.path {
        return Ok(false);
    }
    println!("{}\n  -> {}", item.path.display(), destination.display());
    if pretend {
        return Ok(true);
    }
    let sidecars = naming::sidecars(&item.path);
    if let Err(err) = fileops::transfer(&item.path, &destination, Operation::Move) {
        eprintln!("  error: {err:#}");
        return Ok(false);
    }
    for (sidecar, suffix) in sidecars {
        let target = naming::sidecar_destination(&destination, &suffix);
        if let Err(err) = fileops::transfer(&sidecar, &target, Operation::Move) {
            eprintln!("  warning: {err:#}");
        }
    }
    prune(config, item);
    item.path = destination;
    Ok(true)
}

fn prune(config: &Config, item: &Item) {
    let root = naming::library_root(config, item.kind);
    if let Some(parent) = item.path.parent() {
        fileops::prune_empty_dirs(parent, root);
    }
}

/// Reconciles the database with the files on disk.
pub fn update(
    library: &mut Library,
    query: &Query,
    pretend: bool,
    force: bool,
    assume_yes: bool,
) -> Result<()> {
    let mut missing = Vec::new();
    let mut refreshed = 0;
    for mut item in library.query(query)? {
        if !item.path.exists() {
            println!("missing: {}", item.path.display());
            missing.push(item);
            continue;
        }
        let stamp = file_stamp(&item.path)?;
        if !force && stamp == (item.size, item.mtime) {
            continue;
        }
        println!("changed: {}", item.path.display());
        refreshed += 1;
        if pretend {
            continue;
        }
        (item.size, item.mtime) = stamp;
        match probe::probe(&item.path) {
            Ok(media) => item.media = media,
            Err(err) => eprintln!("  warning: {err:#}"),
        }
        library.save(&mut item)?;
    }

    if pretend {
        println!(
            "Would remove {} missing file(s); would refresh {refreshed}.",
            missing.len()
        );
        return Ok(());
    }
    let confirmed = missing.is_empty()
        || assume_yes
        || prompt(&format!(
            "Remove {} missing file(s) from the library? [y/N] ",
            missing.len()
        ))?
        .is_some_and(|answer| answer.eq_ignore_ascii_case("y"));
    let removed = if confirmed {
        for id in missing.iter().filter_map(|item| item.id) {
            library.remove(id)?;
        }
        missing.len()
    } else {
        0
    };
    println!("Removed {removed} missing file(s); refreshed {refreshed}.");
    Ok(())
}

pub fn remove(
    config: &Config,
    library: &mut Library,
    query: &Query,
    delete: bool,
    assume_yes: bool,
) -> Result<()> {
    let items = library.query(query)?;
    if items.is_empty() {
        println!("No matching items.");
        return Ok(());
    }
    for item in &items {
        println!("{item}");
    }
    remove_items(config, library, &items, delete, assume_yes)
}

/// Removes items whose metadata lacks what they need to be identified and named.
pub fn purge(
    config: &Config,
    library: &mut Library,
    query: &Query,
    delete: bool,
    assume_yes: bool,
) -> Result<()> {
    let incomplete: Vec<Item> = library
        .query(query)?
        .into_iter()
        .filter(|item| !item.missing_metadata().is_empty())
        .collect();
    if incomplete.is_empty() {
        println!("No items with incomplete metadata.");
        return Ok(());
    }
    for item in &incomplete {
        println!(
            "{} (no {})\n  {}",
            item,
            item.missing_metadata().join(", "),
            item.path.display()
        );
    }
    remove_items(config, library, &incomplete, delete, assume_yes)
}

/// Asks, unless `assume_yes`, then drops the items' rows and, with `delete`, their files.
fn remove_items(
    config: &Config,
    library: &mut Library,
    items: &[Item],
    delete: bool,
    assume_yes: bool,
) -> Result<()> {
    if !assume_yes {
        let action = if delete {
            "Remove from the library AND DELETE the files"
        } else {
            "Remove from the library (files are kept)"
        };
        let answer = prompt(&format!("{action}: {} item(s)? [y/N] ", items.len()))?;
        if !answer.is_some_and(|a| a.eq_ignore_ascii_case("y")) {
            println!("Nothing removed.");
            return Ok(());
        }
    }
    for item in items {
        if delete {
            let sidecars = naming::sidecars(&item.path);
            for path in std::iter::once(item.path.clone()).chain(sidecars.into_iter().map(|s| s.0))
            {
                if let Err(err) = std::fs::remove_file(&path)
                    && err.kind() != std::io::ErrorKind::NotFound
                {
                    eprintln!("warning: cannot delete {}: {err}", path.display());
                }
            }
            prune(config, item);
        }
        if let Some(id) = item.id {
            library.remove(id)?;
        }
    }
    println!("Removed {} item(s).", items.len());
    Ok(())
}

fn tally<'a>(items: &'a [Item], key: impl Fn(&'a Item) -> Option<String>) -> Vec<(String, usize)> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for item in items {
        *counts
            .entry(key(item).unwrap_or_else(|| "unknown".into()))
            .or_default() += 1;
    }
    let mut counts: Vec<_> = counts.into_iter().collect();
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    counts
}

fn print_tally(heading: &str, counts: Vec<(String, usize)>) {
    if counts.iter().all(|(name, _)| name == "unknown") {
        return;
    }
    println!("\n{heading}:");
    for (name, count) in counts {
        println!("  {name:<24} {count}");
    }
}

pub fn stats(library: &Library, query: &Query) -> Result<()> {
    let items = library.query(query)?;
    let identity = |item: &Item| (item.title.clone(), item.year, item.source_id.clone());
    let distinct = |kind: Kind| {
        items
            .iter()
            .filter(|item| item.kind == kind)
            .map(identity)
            .collect::<HashSet<_>>()
            .len()
    };
    let files = |kind: Kind| items.iter().filter(|item| item.kind == kind).count();

    println!(
        "Films:      {} ({} files)",
        distinct(Kind::Movie),
        files(Kind::Movie)
    );
    println!(
        "Shows:      {} ({} episode files)",
        distinct(Kind::Episode),
        files(Kind::Episode)
    );
    println!(
        "Total size: {}",
        human_size(items.iter().map(|item| item.size).sum())
    );
    println!(
        "Total time: {}",
        human_duration(items.iter().filter_map(|item| item.media.duration).sum())
    );

    print_tally(
        "Resolution",
        tally(&items, |item| item.media.resolution.clone()),
    );
    print_tally(
        "Dynamic range",
        tally(&items, |item| item.media.hdr.clone()),
    );
    let dolby_vision: Vec<Item> = items
        .iter()
        .filter(|item| item.media.dv_profile.is_some())
        .cloned()
        .collect();
    print_tally(
        "Dolby Vision profile",
        tally(&dolby_vision, |item| item.media.dv_profile.clone()),
    );
    print_tally(
        "Video codec",
        tally(&items, |item| item.media.video_codec.clone()),
    );
    print_tally(
        "Primary audio",
        tally(&items, |item| {
            let stream = item.media.audio.first()?;
            let mut name = stream.codec.clone()?;
            if let Some(layout) = &stream.layout {
                name.push_str(&format!(" {layout}"));
            }
            if stream.atmos {
                name.push_str(" Atmos");
            }
            Some(name)
        }),
    );
    let atmos = items
        .iter()
        .filter(|item| item.media.audio.iter().any(|stream| stream.atmos))
        .count();
    println!("\nFiles with Dolby Atmos: {atmos}");
    Ok(())
}

pub fn config(config: &Config, path: &Path, show_path: bool, init: bool) -> Result<()> {
    if show_path {
        println!("{}", path.display());
    } else if init {
        if path.exists() {
            anyhow::bail!("{} already exists", path.display());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, Config::default().to_toml()?)?;
        println!("Wrote {}", path.display());
    } else {
        print!("{}", config.redacted().to_toml()?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;
    use crate::model::{AudioStream, SubtitleStream};

    struct Fixture {
        dir: tempfile::TempDir,
        config: Config,
        library: Library,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let mut config = Config::default();
            config.library.movies = dir.path().join("Movies");
            config.library.tv = dir.path().join("TV");
            Fixture {
                dir,
                config,
                library: Library::in_memory().unwrap(),
            }
        }

        fn write(&self, relative: &str, contents: &str) -> PathBuf {
            let path = self.dir.path().join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, contents).unwrap();
            path
        }

        /// Adds Heat (1995) as an identified film whose file is at `relative`.
        fn film(&mut self, relative: &str, resolution: Option<&str>) -> Item {
            let path = self.write(relative, "video");
            let mut item = Item::new(Kind::Movie, path.clone(), "Heat".into());
            item.year = Some(1995);
            item.source = Some("tmdb".into());
            item.source_id = Some("949".into());
            item.media.resolution = resolution.map(str::to_owned);
            (item.size, item.mtime) = file_stamp(&path).unwrap();
            self.library.save(&mut item).unwrap();
            item
        }

        fn rows(&self) -> Vec<Item> {
            self.library.query(&Query::default()).unwrap()
        }

        fn paths(&self) -> Vec<PathBuf> {
            self.rows().into_iter().map(|item| item.path).collect()
        }

        fn movies(&self, relative: &str) -> PathBuf {
            self.config.library.movies.join(relative)
        }
    }

    fn query(terms: &[&str]) -> Query {
        Query::parse(terms).unwrap()
    }

    #[test]
    fn sizes_are_human_readable() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(1536), "1.5 KiB");
        assert_eq!(human_size(5 * 1024 * 1024 * 1024), "5.0 GiB");
        assert_eq!(human_size(u64::MAX), "16777216.0 TiB");
    }

    #[test]
    fn durations_are_human_readable() {
        assert_eq!(human_duration(0.0), "0h 00m");
        assert_eq!(human_duration(59.6), "0h 01m");
        assert_eq!(human_duration(3661.0), "1h 01m");
        assert_eq!(human_duration(90_000.0), "1d 1h 00m");
    }

    #[test]
    fn tallies_sort_by_count_then_name_and_label_gaps() {
        let items: Vec<Item> = [
            Some("1080p"),
            Some("2160p"),
            None,
            Some("1080p"),
            Some("720p"),
        ]
        .into_iter()
        .map(|resolution| {
            let mut item = Item::new(Kind::Movie, "/x.mkv".into(), "X".into());
            item.media.resolution = resolution.map(str::to_owned);
            item
        })
        .collect();
        let counts = tally(&items, |item| item.media.resolution.clone());
        assert_eq!(
            counts,
            [
                ("1080p".to_string(), 2),
                ("2160p".to_string(), 1),
                ("720p".to_string(), 1),
                ("unknown".to_string(), 1),
            ]
        );
    }

    #[test]
    fn move_renames_the_file_and_its_subtitles_and_updates_the_database() {
        let mut fixture = Fixture::new();
        fixture.film("Movies/messy/heat.mkv", None);
        fixture.write("Movies/messy/heat.en.srt", "sub");
        fixture.write("Movies/messy/notes.txt", "keep");

        move_items(&fixture.config, &mut fixture.library, &query(&[]), false).unwrap();

        let video = fixture.movies("Heat (1995) {tmdb-949}/Heat (1995) {tmdb-949}.mkv");
        assert_eq!(fixture.paths(), std::slice::from_ref(&video));
        assert!(video.exists());
        assert!(video.with_extension("en.srt").exists());
        assert!(!fixture.movies("messy/heat.mkv").exists());
        assert!(!fixture.movies("messy/heat.en.srt").exists());
        assert!(fixture.movies("messy/notes.txt").exists());
    }

    #[test]
    fn move_prunes_directories_it_empties_but_not_the_library_root() {
        let mut fixture = Fixture::new();
        fixture.film("Movies/old/name/heat.mkv", None);
        move_items(&fixture.config, &mut fixture.library, &query(&[]), false).unwrap();
        assert!(!fixture.movies("old").exists());
        assert!(fixture.config.library.movies.is_dir());
    }

    #[test]
    fn pretend_move_changes_nothing() {
        let mut fixture = Fixture::new();
        let item = fixture.film("Movies/messy/heat.mkv", None);
        move_items(&fixture.config, &mut fixture.library, &query(&[]), true).unwrap();
        assert!(item.path.exists());
        assert_eq!(fixture.paths(), [item.path]);
        assert!(!fixture.movies("Heat (1995) {tmdb-949}").exists());
    }

    #[test]
    fn move_leaves_correctly_named_files_alone() {
        let mut fixture = Fixture::new();
        let item = fixture.film(
            "Movies/Heat (1995) {tmdb-949}/Heat (1995) {tmdb-949}.mkv",
            None,
        );
        move_items(&fixture.config, &mut fixture.library, &query(&[]), false).unwrap();
        assert_eq!(fixture.paths(), std::slice::from_ref(&item.path));
        assert!(item.path.exists());
    }

    #[test]
    fn move_only_touches_matching_items() {
        let mut fixture = Fixture::new();
        let heat = fixture.film("Movies/messy/heat.mkv", None);
        let mut other = Item::new(
            Kind::Movie,
            fixture.write("Movies/messy/ronin.mkv", "video"),
            "Ronin".into(),
        );
        other.year = Some(1998);
        fixture.library.save(&mut other).unwrap();

        move_items(
            &fixture.config,
            &mut fixture.library,
            &query(&["ronin"]),
            false,
        )
        .unwrap();

        assert!(heat.path.exists(), "Heat did not match the query");
        assert!(fixture.movies("Ronin (1998)/Ronin (1998).mkv").exists());
    }

    #[test]
    fn move_adds_version_suffixes_to_every_copy() {
        let mut fixture = Fixture::new();
        fixture.film(
            "Movies/Heat (1995) {tmdb-949}/Heat (1995) {tmdb-949}.mkv",
            Some("1080p"),
        );
        let mut better = fixture.film("Movies/downloads/heat-uhd.mkv", Some("2160p"));
        better.size += 1;
        fixture.library.save(&mut better).unwrap();

        move_items(&fixture.config, &mut fixture.library, &query(&[]), false).unwrap();

        let mut paths = fixture.paths();
        paths.sort();
        assert_eq!(
            paths,
            [
                fixture.movies("Heat (1995) {tmdb-949}/Heat (1995) {tmdb-949} - 1080p.mkv"),
                fixture.movies("Heat (1995) {tmdb-949}/Heat (1995) {tmdb-949} - 2160p.mkv"),
            ]
        );
        assert!(paths.iter().all(|path| path.exists()));
    }

    #[test]
    fn move_numbers_copies_that_would_share_a_name() {
        let mut fixture = Fixture::new();
        fixture.film("Movies/a/heat.mkv", Some("1080p"));
        let mut second = fixture.film("Movies/b/heat.mkv", Some("1080p"));
        second.size += 1;
        fixture.library.save(&mut second).unwrap();

        move_items(&fixture.config, &mut fixture.library, &query(&[]), false).unwrap();

        let mut paths = fixture.paths();
        paths.sort();
        assert_eq!(
            paths,
            [
                fixture.movies("Heat (1995) {tmdb-949}/Heat (1995) {tmdb-949} - 1080p copy 2.mkv"),
                fixture.movies("Heat (1995) {tmdb-949}/Heat (1995) {tmdb-949} - 1080p.mkv"),
            ]
        );
        assert!(paths.iter().all(|path| path.exists()));
    }

    #[test]
    fn move_survives_a_missing_file() {
        let mut fixture = Fixture::new();
        let item = fixture.film("Movies/messy/heat.mkv", None);
        fs::remove_file(&item.path).unwrap();
        move_items(&fixture.config, &mut fixture.library, &query(&[]), false).unwrap();
        assert_eq!(fixture.paths(), [item.path], "the row is left as it was");
    }

    #[test]
    fn update_leaves_unchanged_files_alone() {
        let mut fixture = Fixture::new();
        let item = fixture.film("Movies/heat.mkv", Some("1080p"));
        update(&mut fixture.library, &query(&[]), false, false, true).unwrap();
        let rows = fixture.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].size, rows[0].mtime), (item.size, item.mtime));
        assert_eq!(rows[0].media.resolution.as_deref(), Some("1080p"));
    }

    #[test]
    fn update_notices_changed_files() {
        let mut fixture = Fixture::new();
        let item = fixture.film("Movies/heat.mkv", None);
        fs::write(&item.path, "a much longer video").unwrap();
        update(&mut fixture.library, &query(&[]), false, false, true).unwrap();
        assert_eq!(fixture.rows()[0].size, "a much longer video".len() as u64);
    }

    #[test]
    fn pretend_update_changes_nothing() {
        let mut fixture = Fixture::new();
        let changed = fixture.film("Movies/changed.mkv", None);
        fs::write(&changed.path, "a much longer video").unwrap();
        let mut missing = Item::new(
            Kind::Movie,
            fixture.dir.path().join("gone.mkv"),
            "Gone".into(),
        );
        fixture.library.save(&mut missing).unwrap();

        update(&mut fixture.library, &query(&[]), true, false, true).unwrap();

        assert_eq!(fixture.rows().len(), 2);
        let row = fixture.library.query(&query(&["heat"])).unwrap().remove(0);
        assert_eq!(row.size, changed.size);
    }

    #[test]
    fn update_removes_rows_for_missing_files_when_confirmed() {
        let mut fixture = Fixture::new();
        let kept = fixture.film("Movies/heat.mkv", None);
        let mut gone = Item::new(
            Kind::Movie,
            fixture.dir.path().join("gone.mkv"),
            "Gone".into(),
        );
        fixture.library.save(&mut gone).unwrap();

        update(&mut fixture.library, &query(&[]), false, false, true).unwrap();

        assert_eq!(fixture.paths(), [kept.path]);
    }

    #[test]
    fn update_only_considers_matching_items() {
        let mut fixture = Fixture::new();
        fixture.film("Movies/heat.mkv", None);
        let mut gone = Item::new(
            Kind::Movie,
            fixture.dir.path().join("gone.mkv"),
            "Gone".into(),
        );
        fixture.library.save(&mut gone).unwrap();

        update(&mut fixture.library, &query(&["heat"]), false, false, true).unwrap();

        assert_eq!(fixture.rows().len(), 2);
    }

    #[test]
    fn remove_drops_rows_and_keeps_files_by_default() {
        let mut fixture = Fixture::new();
        let heat = fixture.film("Movies/heat.mkv", None);
        let mut ronin = Item::new(
            Kind::Movie,
            fixture.write("Movies/ronin.mkv", "x"),
            "Ronin".into(),
        );
        fixture.library.save(&mut ronin).unwrap();

        remove(
            &fixture.config,
            &mut fixture.library,
            &query(&["heat"]),
            false,
            true,
        )
        .unwrap();

        assert_eq!(fixture.paths(), [ronin.path]);
        assert!(heat.path.exists());
    }

    #[test]
    fn items_lacking_identifying_metadata_are_incomplete() {
        let mut film = Item::new(Kind::Movie, "/m.mkv".into(), "Heat".into());
        assert_eq!(film.missing_metadata(), ["source", "year"]);
        (film.source, film.source_id, film.year) =
            (Some("tmdb".into()), Some("949".into()), Some(1995));
        assert!(film.missing_metadata().is_empty());

        let mut episode = Item::new(Kind::Episode, "/e.mkv".into(), "Show".into());
        (episode.source, episode.source_id) = (Some("tvdb".into()), Some("1".into()));
        episode.season = Some(1);
        assert_eq!(episode.missing_metadata(), ["episode"]);
        episode.episode = Some(1);
        assert!(
            episode.missing_metadata().is_empty(),
            "titles may be unknown"
        );

        episode.date_based = true;
        assert_eq!(episode.missing_metadata(), ["air date"]);
    }

    #[test]
    fn purge_removes_only_incomplete_items() {
        let mut fixture = Fixture::new();
        let heat = fixture.film("Movies/heat.mkv", None);
        let mut unidentified = Item::new(
            Kind::Movie,
            fixture.write("Movies/ronin.mkv", "x"),
            "Ronin".into(),
        );
        unidentified.year = Some(1998);
        fixture.library.save(&mut unidentified).unwrap();

        purge(
            &fixture.config,
            &mut fixture.library,
            &Query::default(),
            true,
            true,
        )
        .unwrap();

        assert_eq!(fixture.paths(), [heat.path]);
        assert!(!unidentified.path.exists());
    }

    #[test]
    fn remove_with_delete_removes_files_sidecars_and_empty_directories() {
        let mut fixture = Fixture::new();
        let heat = fixture.film("Movies/Heat (1995)/Heat (1995).mkv", None);
        let subtitle = fixture.write("Movies/Heat (1995)/Heat (1995).en.srt", "sub");
        let other = fixture.write("Movies/Heat (1995)/Heat (1995) trailer.txt", "unrelated");
        fs::remove_file(&other).unwrap();

        remove(
            &fixture.config,
            &mut fixture.library,
            &query(&["heat"]),
            true,
            true,
        )
        .unwrap();

        assert!(fixture.rows().is_empty());
        assert!(!heat.path.exists());
        assert!(!subtitle.exists());
        assert!(!fixture.movies("Heat (1995)").exists());
        assert!(fixture.config.library.movies.is_dir());
    }

    #[test]
    fn remove_with_delete_keeps_unrelated_files_in_the_directory() {
        let mut fixture = Fixture::new();
        let heat = fixture.film("Movies/Heat (1995)/Heat (1995).mkv", None);
        let poster = fixture.write("Movies/Heat (1995)/poster.jpg", "art");

        remove(
            &fixture.config,
            &mut fixture.library,
            &query(&["heat"]),
            true,
            true,
        )
        .unwrap();

        assert!(!heat.path.exists());
        assert!(poster.exists());
    }

    #[test]
    fn remove_with_delete_tolerates_files_that_are_already_gone() {
        let mut fixture = Fixture::new();
        let heat = fixture.film("Movies/heat.mkv", None);
        fs::remove_file(&heat.path).unwrap();
        remove(
            &fixture.config,
            &mut fixture.library,
            &query(&["heat"]),
            true,
            true,
        )
        .unwrap();
        assert!(fixture.rows().is_empty());
    }

    #[test]
    fn remove_without_matches_does_nothing() {
        let mut fixture = Fixture::new();
        fixture.film("Movies/heat.mkv", None);
        remove(
            &fixture.config,
            &mut fixture.library,
            &query(&["nonexistent"]),
            true,
            true,
        )
        .unwrap();
        assert_eq!(fixture.rows().len(), 1);
        assert!(fixture.rows()[0].path.exists());
    }

    #[test]
    fn listing_info_and_stats_handle_fully_populated_items() {
        let mut fixture = Fixture::new();
        let mut item = fixture.film("Movies/heat.mkv", Some("2160p"));
        item.original_path = Some("/downloads/heat.mkv".into());
        item.media.duration = Some(10_200.0);
        item.media.hdr = Some("DV/HDR10".into());
        item.media.dv_profile = Some("7 FEL".into());
        item.media.video_codec = Some("hevc".into());
        item.media.colour_space = Some("bt2020nc".into());
        item.media.audio = vec![AudioStream {
            codec: Some("truehd".into()),
            layout: Some("7.1".into()),
            atmos: true,
            ..AudioStream::default()
        }];
        item.media.subtitles = vec![SubtitleStream {
            language: Some("eng".into()),
            sdh: true,
            ..SubtitleStream::default()
        }];
        fixture.library.save(&mut item).unwrap();
        fixture.write("Movies/heat.en.srt", "sub");

        let everything = query(&[]);
        list(&fixture.library, &everything, None, false).unwrap();
        list(&fixture.library, &everything, None, true).unwrap();
        list(&fixture.library, &everything, Some("$title"), false).unwrap();
        info(&fixture.library, &everything).unwrap();
        stats(&fixture.library, &everything).unwrap();
        stats(&Library::in_memory().unwrap(), &everything).unwrap();
    }

    #[test]
    fn config_init_writes_a_loadable_file_and_refuses_to_overwrite_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/config.toml");
        let config = Config::default();

        super::config(&config, &path, false, true).unwrap();
        assert!(Config::load(&path).is_ok());

        let before = fs::read_to_string(&path).unwrap();
        let error = super::config(&config, &path, false, true).unwrap_err();
        assert!(error.to_string().contains("already exists"));
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn config_shows_the_path_or_the_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        super::config(&Config::default(), &path, true, false).unwrap();
        super::config(&Config::default(), &path, false, false).unwrap();
        assert!(!path.exists(), "showing never writes");
    }
}
