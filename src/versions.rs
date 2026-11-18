//! Deciding what to do with a new file of a film or episode the library already has.

use std::path::Path;

use anyhow::Result;

use crate::commands::{describe_audio, describe_subtitle, human_duration, human_size};
use crate::import::paint;
use crate::model::Item;

/// Decides what to do with a new file (an item, and where it is) given the library's other
/// files of the same film or episode.
pub type Resolve<'a> = &'a dyn Fn(&Item, &Path, &[Item]) -> Result<Choice>;

/// What to do with a new file of something the library already has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// Import it as another version, as when nobody is asked.
    KeepBoth,
    /// Leave it out.
    Skip,
    /// Import it in place of the existing file at this index, which is deleted.
    Replace(usize),
    /// Stop the import.
    Abort,
}

/// Shows `new` (found at `path`) beside the library's `existing` files of the same film or
/// episode, and asks what to do with it.
pub fn ask(
    new: &Item,
    path: &Path,
    existing: &[Item],
    prompt: impl Fn(&str) -> Result<Option<String>>,
) -> Result<Choice> {
    println!("  The library already has this:");
    print!("{}", table(new, path, existing, textwrap::termwidth()));
    let which = match existing.len() {
        1 => String::new(),
        n => format!(" (r1–r{n})"),
    };
    loop {
        let Some(answer) = prompt(&format!("  [k]eep both, [r]eplace{which}, or [s]kip? "))? else {
            return Ok(Choice::Abort);
        };
        match parse(&answer.to_lowercase(), existing.len()) {
            Some(choice) => return Ok(choice),
            None => println!("  Unrecognised choice."),
        }
    }
}

fn parse(answer: &str, existing: usize) -> Option<Choice> {
    match answer {
        "k" | "keep" => Some(Choice::KeepBoth),
        "s" | "skip" => Some(Choice::Skip),
        "r" | "replace" if existing == 1 => Some(Choice::Replace(0)),
        _ => {
            let number: usize = answer.strip_prefix('r')?.parse().ok()?;
            (1..=existing)
                .contains(&number)
                .then(|| Choice::Replace(number - 1))
        }
    }
}

/// Each file's details side by side, one column per file, fitted to `width` columns. Rows on
/// which every file agrees are dimmed, so the differences stand out.
fn table(new: &Item, path: &Path, existing: &[Item], width: usize) -> String {
    let mut headings = vec!["new".to_string()];
    headings.extend((1..=existing.len()).map(|n| match existing.len() {
        1 => "library".to_string(),
        _ => format!("library {n}"),
    }));
    let files: Vec<(&Item, &Path)> = std::iter::once((new, path))
        .chain(existing.iter().map(|item| (item, item.path.as_path())))
        .collect();
    let rows: Vec<(&str, Vec<Vec<String>>)> = ROWS
        .iter()
        .map(|&(label, cell)| {
            let cells = files.iter().map(|&(item, path)| cell(item, path)).collect();
            (label, cells)
        })
        .filter(|(_, cells): &(_, Vec<Vec<String>>)| cells.iter().any(|lines| !lines.is_empty()))
        .collect();

    let label_width = rows.iter().map(|(label, _)| label.len()).max().unwrap_or(0);
    let gap = 2;
    let available = width.saturating_sub(2 + label_width + gap * files.len()) / files.len();
    let column_width = (0..files.len())
        .map(|column| {
            let widest = rows
                .iter()
                .flat_map(|(_, cells)| &cells[column])
                .chain([&headings[column]])
                .map(|line| line.chars().count())
                .max()
                .unwrap_or(0);
            widest.min(available.max(12))
        })
        .collect::<Vec<_>>();

    let mut out = String::new();
    let line = |label: &str, cells: Vec<&str>| {
        let mut text = format!("  {label:<label_width$}");
        for (cell, &width) in cells.iter().zip(&column_width) {
            text.push_str(&" ".repeat(gap));
            text.push_str(&pad(&fit(cell, width), width));
        }
        text.truncate(text.trim_end().len());
        text
    };
    out.push_str(&paint("1", &line("", headings.iter().map(String::as_str).collect())));
    out.push('\n');
    for (label, cells) in &rows {
        let agreed = *label != "file" && cells.windows(2).all(|pair| pair[0] == pair[1]);
        let height = cells.iter().map(Vec::len).max().unwrap_or(0).max(1);
        for index in 0..height {
            let parts = cells
                .iter()
                .map(|lines| match (lines.get(index), index) {
                    (Some(text), _) => text.as_str(),
                    (None, 0) => "—",
                    (None, _) => "",
                })
                .collect();
            let text = line(if index == 0 { label } else { "" }, parts);
            out.push_str(&if agreed { paint("2", &text) } else { text });
            out.push('\n');
        }
    }
    out
}

/// The most lines a stream list takes up before the rest are counted instead.
const MOST_STREAMS: usize = 6;

type Cell = fn(&Item, &Path) -> Vec<String>;

const ROWS: &[(&str, Cell)] = &[
    ("file", |_, path| {
        vec![path.file_name().unwrap_or_default().to_string_lossy().into_owned()]
    }),
    ("size", |item, _| vec![human_size(item.size)]),
    ("duration", |item, _| {
        item.media.duration.map(human_duration).into_iter().collect()
    }),
    ("bitrate", |item, _| bitrate(item).into_iter().collect()),
    ("video", |item, _| {
        let media = &item.media;
        let parts = [
            media.video_codec.clone(),
            media.width.zip(media.height).map(|(w, h)| format!("{w}x{h}")),
            media.bit_depth.map(|bits| format!("{bits}-bit")),
        ];
        joined(parts)
    }),
    ("hdr", |item, _| {
        let media = &item.media;
        let profile = media.dv_profile.as_ref().map(|p| format!("DV profile {p}"));
        joined([media.hdr.clone(), profile])
    }),
    ("colour", |item, _| {
        let media = &item.media;
        let parts = [
            media.colour_space.clone(),
            media.colour_primaries.clone(),
            media.transfer.clone(),
        ];
        if parts.iter().all(Option::is_none) {
            return Vec::new();
        }
        let parts = parts.map(|part| part.unwrap_or_else(|| "?".into()));
        vec![parts.join(" / ")]
    }),
    ("audio", |item, _| {
        listed(item.media.audio.iter().map(describe_audio).collect())
    }),
    ("subtitles", |item, _| {
        listed(item.media.subtitles.iter().map(describe_subtitle).collect())
    }),
    ("title tag", |item, _| item.media.title.clone().into_iter().collect()),
];

/// The overall bitrate, from the file's size and running time.
fn bitrate(item: &Item) -> Option<String> {
    let seconds = item.media.duration.filter(|&seconds| seconds > 0.0)?;
    let megabits = item.size as f64 * 8.0 / seconds / 1e6;
    Some(format!("{megabits:.1} Mb/s"))
}

fn joined<const N: usize>(parts: [Option<String>; N]) -> Vec<String> {
    let parts: Vec<String> = parts.into_iter().flatten().collect();
    match parts.is_empty() {
        true => Vec::new(),
        false => vec![parts.join(", ")],
    }
}

fn listed(mut lines: Vec<String>) -> Vec<String> {
    if lines.len() > MOST_STREAMS {
        let more = lines.len() - (MOST_STREAMS - 1);
        lines.truncate(MOST_STREAMS - 1);
        lines.push(format!("… {more} more"));
    }
    lines
}

/// `text` cut to `width` characters, ending in an ellipsis when cut.
fn fit(text: &str, width: usize) -> String {
    match text.chars().count() > width {
        true => text.chars().take(width.saturating_sub(1)).chain(['…']).collect(),
        false => text.to_string(),
    }
}

fn pad(text: &str, width: usize) -> String {
    let fill = width.saturating_sub(text.chars().count());
    format!("{text}{}", " ".repeat(fill))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::model::{AudioStream, Kind};

    fn film(path: &str, size: u64, codec: &str, hdr: Option<&str>) -> Item {
        let mut item = Item::new(Kind::Movie, path.into(), "Heat".into());
        item.size = size;
        item.media.duration = Some(10_000.0);
        item.media.video_codec = Some(codec.into());
        item.media.width = Some(3840);
        item.media.height = Some(2160);
        item.media.hdr = hdr.map(str::to_owned);
        item.media.audio = vec![AudioStream {
            language: Some("eng".into()),
            codec: Some("truehd".into()),
            layout: Some("7.1".into()),
            atmos: true,
            ..AudioStream::default()
        }];
        item
    }

    #[test]
    fn the_table_shows_each_file_side_by_side() {
        let new = film("/in/heat.mkv", 50_000_000_000, "hevc", Some("DV, HDR10"));
        let old = film("/lib/Heat (1995).mkv", 25_000_000_000, "h264", None);
        let table = table(&new, &new.path, &[old], 100);
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines[0].split_whitespace().collect::<Vec<_>>(), ["new", "library"]);
        let row = |label: &str| {
            *lines
                .iter()
                .find(|line| line.trim_start().starts_with(label))
                .unwrap_or_else(|| panic!("no {label} row in\n{table}"))
        };
        assert!(row("file").contains("heat.mkv") && row("file").contains("Heat (1995).mkv"));
        assert!(row("bitrate").contains("40.0 Mb/s") && row("bitrate").contains("20.0 Mb/s"));
        assert!(row("hdr").contains("DV, HDR10") && row("hdr").contains("—"));
        assert!(row("audio").matches("eng, truehd, 7.1, Atmos").count() == 2);
        assert!(!table.contains("title tag"), "rows nobody has are left out");
    }

    #[test]
    fn cells_are_cut_to_fit_the_width() {
        let new = film("/in/a-very-long-release-name-that-goes-on-and-on.mkv", 1, "hevc", None);
        let old = film("/lib/another-very-long-library-file-name-indeed.mkv", 2, "hevc", None);
        let table = table(&new, &new.path, &[old], 60);
        assert!(table.lines().all(|line| line.chars().count() <= 60), "{table}");
        assert!(table.contains('…'));
    }

    #[test]
    fn answers_are_parsed() {
        assert_eq!(parse("k", 1), Some(Choice::KeepBoth));
        assert_eq!(parse("skip", 1), Some(Choice::Skip));
        assert_eq!(parse("r", 1), Some(Choice::Replace(0)));
        assert_eq!(parse("r", 2), None, "which one must be said");
        assert_eq!(parse("r2", 2), Some(Choice::Replace(1)));
        assert_eq!(parse("r3", 2), None);
        assert_eq!(parse("", 1), None);
    }

    #[test]
    fn asking_repeats_until_understood_and_stops_on_ctrl_c() {
        let new = film("/in/heat.mkv", 1, "hevc", None);
        let old = film("/lib/heat.mkv", 2, "hevc", None);
        let answers = RefCell::new(vec![Some("x"), Some("r")]);
        let prompt = |_: &str| Ok(answers.borrow_mut().remove(0).map(str::to_owned));
        let choice = ask(&new, &new.path, std::slice::from_ref(&old), prompt).unwrap();
        assert_eq!(choice, Choice::Replace(0));
        let choice = ask(&new, &new.path, &[old], |_| Ok(None)).unwrap();
        assert_eq!(choice, Choice::Abort);
    }
}
