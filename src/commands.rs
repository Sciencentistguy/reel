//! Library maintenance commands: list, info, move, update, remove and stats.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

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

fn human_duration(seconds: f64) -> String {
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
        let versioned = library.has_other_versions(&item)?;
        let destination = naming::free_destination(config, &item, versioned, &item.path, |path| {
            claimed.contains(path) || path.symlink_metadata().is_ok()
        });
        claimed.insert(destination.clone());
        if destination == item.path {
            continue;
        }
        println!("{}\n  -> {}", item.path.display(), destination.display());
        if pretend {
            moved += 1;
            continue;
        }
        let sidecars = naming::sidecars(&item.path);
        if let Err(err) = fileops::transfer(&item.path, &destination, Operation::Move) {
            eprintln!("  error: {err:#}");
            continue;
        }
        for (sidecar, suffix) in sidecars {
            let target = naming::sidecar_destination(&destination, &suffix);
            if let Err(err) = fileops::transfer(&sidecar, &target, Operation::Move) {
                eprintln!("  warning: {err:#}");
            }
        }
        prune(config, &item);
        item.path = destination;
        library.save(&mut item)?;
        moved += 1;
    }
    let verb = if pretend { "Would move" } else { "Moved" };
    println!("{verb} {moved} file(s).");
    Ok(())
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
    for item in &items {
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
