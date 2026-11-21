//! `reel unmatched`: video files in the library folders that the database has no row for.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::config::Config;
use crate::import;
use crate::library::Library;
use crate::model::Item;
use crate::naming;
use crate::query::Query;

pub fn run(config: &Config, library: &Library) -> Result<()> {
    let items = library.query(&Query::default())?;
    let folders = &config.library;
    let mut roots: Vec<_> = [&folders.movies, &folders.tv, &folders.sport]
        .into_iter()
        .map(|root| LibraryFolder::new(root))
        .collect();
    roots.dedup_by(|a, b| a.canonical.is_some() && a.canonical == b.canonical);
    let mut total = 0;

    for folder in &roots {
        let known =
            folder.labels(items.iter().filter(|item| {
                roots.len() == 1 || naming::library_root(config, item) == folder.root
            }));
        for (top, files) in folder.untracked(library)? {
            let (location, note) = match &top {
                Some(name) => {
                    let label = known
                        .get(name)
                        .map_or("nothing in the library".to_string(), Clone::clone);
                    (
                        folder.root.join(name),
                        format!("{label}, {} file(s)", files.len()),
                    )
                }
                None => (
                    folder.root.clone(),
                    format!("{} loose file(s)", files.len()),
                ),
            };
            println!("{} - {note}", location.display());
            for path in &files {
                let shown = path
                    .strip_prefix(folder.resolved().join(top.as_deref().unwrap_or_default()))
                    .unwrap_or(path);
                println!("    {}", shown.display());
            }
            total += files.len();
        }
    }

    match total {
        0 => println!("Every video file in the library folders is in the library."),
        total => println!("{total} file(s) are not in the library; `reel rescan` adds them."),
    }
    Ok(())
}

/// One of the library's root folders, where each film or show has a folder of its own.
struct LibraryFolder {
    root: PathBuf,
    /// `root` with symlinks resolved, as found files' paths are; `None` when it doesn't exist.
    canonical: Option<PathBuf>,
}

impl LibraryFolder {
    fn new(root: &Path) -> Self {
        LibraryFolder {
            root: root.to_path_buf(),
            canonical: std::fs::canonicalize(root).ok(),
        }
    }

    fn resolved(&self) -> &Path {
        self.canonical.as_deref().unwrap_or(&self.root)
    }

    /// `path` relative to the root, whether or not symlinks in it are resolved.
    fn relative<'a>(&self, path: &'a Path) -> Option<&'a Path> {
        self.canonical
            .as_deref()
            .and_then(|canonical| path.strip_prefix(canonical).ok())
            .or_else(|| path.strip_prefix(&self.root).ok())
    }

    /// The name of the top-level folder holding `path`; `None` for a file directly in the root
    /// or outside it.
    fn top_folder(&self, path: &Path) -> Option<OsString> {
        let relative = self.relative(path)?;
        let mut components = relative.components();
        let first = components.next()?;
        components
            .next()
            .is_some()
            .then(|| first.as_os_str().to_owned())
    }

    /// What the library has in each top-level folder, as `Title (year) [source-id]`.
    fn labels<'a>(&self, items: impl Iterator<Item = &'a Item>) -> HashMap<OsString, String> {
        let mut labels = HashMap::new();
        for item in items {
            if let Some(folder) = self.top_folder(&item.path) {
                labels.entry(folder).or_insert_with(|| label(item));
            }
        }
        labels
    }

    /// Video files under the root that the database has no row for, by top-level folder, with
    /// loose files under `None`.
    fn untracked(&self, library: &Library) -> Result<BTreeMap<Option<OsString>, Vec<PathBuf>>> {
        let mut strays: BTreeMap<_, Vec<_>> = BTreeMap::new();
        if self.canonical.is_none() {
            return Ok(strays);
        }
        let (files, _) = import::videos(std::slice::from_ref(&self.root))?;
        for (path, _) in files {
            let as_configured = self
                .relative(&path)
                .map(|relative| self.root.join(relative));
            if library.contains_path(&path)?
                || as_configured.is_some_and(|path| library.contains_path(&path).unwrap_or(false))
            {
                continue;
            }
            strays.entry(self.top_folder(&path)).or_default().push(path);
        }
        Ok(strays)
    }
}

fn label(item: &Item) -> String {
    let mut label = item.title.clone();
    if let Some(year) = item.year {
        label += &format!(" ({year})");
    }
    if let (Some(source), Some(id)) = (&item.source, &item.source_id) {
        label += &format!(" [{source}-{id}]");
    }
    label
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Kind;

    fn write(dir: &Path, relative: &str) -> PathBuf {
        let path = dir.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "video").unwrap();
        path
    }

    fn episode_at(path: PathBuf) -> Item {
        let mut item = Item::new(Kind::Episode, path, "Show".into());
        (item.season, item.episode) = (Some(1), Some(1));
        (item.source, item.source_id) = (Some("tvdb".into()), Some("1".into()));
        item
    }

    #[test]
    fn files_on_disk_but_not_in_the_database_are_found_by_folder() {
        let dir = tempfile::tempdir().unwrap();
        let known = write(dir.path(), "TV/Show {tvdb-1}/Season 01/Show - s01e01.mkv");
        write(dir.path(), "TV/Show {tvdb-1}/Season 01/Show - s01e02.mkv");
        write(
            dir.path(),
            "TV/Show {tvdb-1}/Season 01/Show - s01e02.en.srt",
        );
        write(dir.path(), "TV/Archer/Archer.S01E01.mkv");
        write(dir.path(), "TV/loose.S01E01.mkv");

        let mut library = Library::in_memory().unwrap();
        library.save(&mut episode_at(known.clone())).unwrap();

        let tv = LibraryFolder::new(&dir.path().join("TV"));
        let found: Vec<_> = tv
            .untracked(&library)
            .unwrap()
            .into_iter()
            .map(|(folder, files)| {
                let names: Vec<_> = files
                    .iter()
                    .map(|path| tv.relative(path).unwrap().to_owned())
                    .collect();
                (folder.map(|f| f.to_string_lossy().into_owned()), names)
            })
            .collect();
        assert_eq!(
            found,
            [
                (None, vec![PathBuf::from("loose.S01E01.mkv")]),
                (
                    Some("Archer".to_string()),
                    vec![PathBuf::from("Archer/Archer.S01E01.mkv")]
                ),
                (
                    Some("Show {tvdb-1}".to_string()),
                    vec![PathBuf::from("Show {tvdb-1}/Season 01/Show - s01e02.mkv")]
                ),
            ],
            "files in the database and non-video files are left out"
        );
    }

    #[test]
    fn folders_are_labelled_with_what_the_library_has_in_them() {
        let tv = LibraryFolder::new(Path::new("/tv"));
        let mut show = episode_at(PathBuf::from("/tv/Show {tvdb-1}/Season 01/a.mkv"));
        show.year = Some(2020);
        let loose = episode_at(PathBuf::from("/tv/b.mkv"));
        let labels = tv.labels([show, loose].iter());
        assert_eq!(labels.len(), 1);
        assert_eq!(
            labels[&OsString::from("Show {tvdb-1}")],
            "Show (2020) [tvdb-1]"
        );
    }

    #[test]
    fn a_missing_library_folder_has_nothing_untracked() {
        let folder = LibraryFolder::new(Path::new("/definitely/not/here"));
        assert!(
            folder
                .untracked(&Library::in_memory().unwrap())
                .unwrap()
                .is_empty()
        );
    }
}
