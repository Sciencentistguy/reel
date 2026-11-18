# reel

A library manager for films and TV shows, in the spirit of [beets](https://github.com/beetbox/beets).
`reel` identifies video files, files them into a directory tree that follows Plex's naming
conventions for [films](https://support.plex.tv/articles/naming-and-organizing-your-movie-media-files/)
and [TV shows](https://support.plex.tv/articles/naming-and-organizing-your-tv-show-files/), and keeps
a queryable SQLite database of what it has imported.

```
Movies/
  Blade Runner (1982) {tmdb-78}/
    Blade Runner (1982) {tmdb-78} {edition-Final Cut}.mkv
    Blade Runner (1982) {tmdb-78} {edition-Final Cut}.en.srt
TV Shows/
  Doctor Who (2005) {tvdb-78804}/
    Season 01/
      Doctor Who (2005) - s01e01 - Rose.mkv
```

## Quickstart

```sh
cargo install --path .  # or: nix profile install . (bundles ffprobe)
reel config --init      # writes ~/.config/reel/config.toml
```

Then edit the configuration:

```toml
[library]
database = "~/.local/share/reel/library.db"
movies = "~/Videos/Movies"
tv = "~/Videos/TV Shows"

[import]
operation = "copy"      # copy | move | hardlink | symlink | reflink
subtitles = true        # bring sidecar subtitles along
probe = true            # record media information with ffprobe
strong_match = 0.08     # matches at or below this distance are applied unprompted
timid = false           # always ask, even about certain matches

[naming]
id_tags = true          # {tmdb-123} / {tvdb-123} folder tags

[metadata]
tv_source = "tvdb"      # or "tmdb"
tmdb_api_key = ""       # v3 key or v4 read access token; or set TMDB_API_KEY
tvdb_api_key = ""       # v4 key; or set TVDB_API_KEY
tmdb_language = "en-US"
tvdb_language = "eng"
```

Films are always matched against TMDB. Shows are matched against TVDB by default, or TMDB with
`tv_source = "tmdb"`. `ffprobe` (from ffmpeg) must be on `PATH` for media information.

API keys can also be kept in separate files with `tmdb_api_key_file` and `tvdb_api_key_file`.
A key is taken from the environment variable first, then the inline setting, then the file.

When a user has no configuration file, `/etc/reel/config.toml` is used if it exists.

### NixOS and home-manager

The flake provides `nixosModules.default` and `homeManagerModules.default`. Both install `reel`
(with `ffprobe`) and generate its configuration from `programs.reel.settings`, which takes the same
keys as `config.toml`. The home-manager module writes `~/.config/reel/config.toml`; the NixOS
module writes the system-wide `/etc/reel/config.toml`.

```nix
{
  imports = [ inputs.reel.homeManagerModules.default ]; # or nixosModules.default

  programs.reel = {
    enable = true;
    settings = {
      library = {
        movies = "/srv/media/Movies";
        tv = "/srv/media/TV Shows";
      };
      import.operation = "hardlink";
      metadata = {
        # The generated file is world-readable in the Nix store: point at secrets, don't inline them.
        tmdb_api_key_file = "/run/secrets/tmdb";
        tvdb_api_key_file = "/run/secrets/tvdb";
      };
    };
  };
}
```

## Command reference

| Command | Purpose |
| --- | --- |
| `reel import PATH...` | Identify files and file them into the library |
| `reel rescan [PATH...]` | Add files already in the library folders but missing from the database |
| `reel list [QUERY]` | List items (`-f` for a template, `-p` for paths) |
| `reel info [QUERY]` | Show everything known about items, including streams |
| `reel move [QUERY]` | Rename files to match the current naming scheme |
| `reel sync [QUERY]` | Refresh titles and episode data from TMDB/TVDB, renaming files to match |
| `reel missing [QUERY]` | List aired episodes of shows with no file (`-s` includes specials) |
| `reel unmatched` | List video files in the library folders that are not in the database |
| `reel update [QUERY]` | Re-probe changed files, drop entries whose files have gone |
| `reel remove QUERY` | Remove items from the library (`-d` also deletes the files) |
| `reel purge [QUERY]` | Remove items lacking a source, year, or episode numbering (`-d` also deletes the files) |
| `reel stats [QUERY]` | Summarise the library |
| `reel fields` | List fields usable in queries and templates |
| `reel config` | Show the effective configuration |

### Importing

`reel import` walks the given paths, guesses a title, year and episode numbering from each file
name (falling back to parent directories), and groups files by film or show. Each group is looked
up once. A confident match is applied automatically; otherwise you choose from the candidates,
search again, enter an ID (or paste a TMDB or TVDB page URL, or a TVDB slug), set the edition for every file in a film's
group (overriding any `{edition-…}` in the names), list the group's files, or skip. `--quiet` never prompts and skips
anything uncertain. `--timid` (or `timid = true`) asks even about certain matches and ID-tagged
names, so with `--quiet` it skips everything; `--no-timid` overrides the configuration. `--pretend` (or `--dry-run`) shows what would happen.
Everything imported is matched to TMDB or TVDB; files that can't be matched are skipped.

An entered ID or URL is shown with its match percentage. A confident match is used straight
away, as with search results; otherwise, or in timid mode, you are asked to confirm it.

When a film is a single file and the match is uncertain, reel compares the file's running time
(from ffprobe, unless `probe = false`) with TMDB's for the best few candidates. That separates
remakes and films that share a name. A difference of up to 6% is ignored, which covers PAL speed-up
and credits. The penalty is lighter for files with an edition, which may legitimately run longer or
shorter. Running times are shown beside the candidates when you are asked to choose.

Shows are checked against their episode lists. A show is only chosen without asking, whether by
name, by an ID tag or by an entered ID, when its list has every file's episode; one that leaves any
file unmatched is offered as a candidate instead, so `--quiet` skips the group. When the match is
uncertain, or the best candidate lacks episodes, the best few candidates' lists are fetched and
each is ranked down for files whose episode it lacks: 2 points for any at all, plus half the share
unmatched, so a six-part miniseries cannot claim a hundred files from the long-running show it
shares a name with. Episode counts, and how many files are unmatched (`3 file(s) unmatched`), are
shown beside the candidates. If you choose a show anyway, files whose episodes it doesn't list are
skipped.

Each episode file's running time is also checked against the episode it is numbered as. Sources
round episode runtimes and often give nominal ones, so only a file shorter than two thirds or longer
than half again as long as its episodes (summed, for a multi-episode file) is rejected: a short
in a special's place, or two episodes filed as one. Such files are skipped with a warning and
show up in `reel unmatched`. Specials are often listed with a placeholder runtime, so `import`,
`rescan` and `sync` take `--skip-length-check` to place files whatever their length.

When you're there to answer, a file that can't be placed is asked about instead of skipped: give
the episode it really is (`s13e05`, `13x05`, `s2006e02`, or an air date such as `2006-02-05`), or
type words from its title or a year to search the show's episode list, which lists each match's
air date and runtime. A blank answer skips the file; Ctrl-C stops the import. This is how films in an anthology series, or
files numbered differently from the source, find their place. `reel sync` applies the same check to items already in the library,
leaving any that fail untouched and listing them.

`reel rescan` rebuilds missing database rows. It walks the library's film and TV folders (or the
paths given), identifies files the database doesn't know about exactly as `import` does, and
records them where they are. Nothing is moved or renamed; run `reel move` afterwards to bring the
names in line with the naming scheme. It accepts the same `--quiet`, `--timid`, `--no-timid`
and `--pretend` flags, and never removes rows (that's `reel update`).

`reel sync` refreshes items that were matched to TMDB or TVDB. It fetches each film or show once
from the source it was matched against (even if `tv_source` has changed since), updates titles,
years, episode titles and air dates, and prints what changed. Files are then renamed to match, as
`reel move` would; `--no-move` (`-M`) only updates the database, and `--pretend` shows what would
happen. Episodes that were "TBA" at import time are the usual beneficiaries. Fields the source
leaves blank keep their current values.

`reel missing` fetches the full episode list of every show among the matching items
and prints the episodes no file covers, grouped by show. Multi-episode files count for each
episode they span, and date-based files are matched by air date. Episodes with a future air
date are left out, as are specials (season 0) unless `--specials` (`-s`) is given.

`reel unmatched` is the other side: video files in the film and TV folders that the database has
no row for, usually ones an import or rescan couldn't match. They are grouped by the film or show
folder they sit in, labelled with what the library has there (or `nothing in the library`), with
loose files in a root listed separately. `reel rescan` tries to match them again.

`reel purge` removes items whose metadata is incomplete: anything not matched to a source, films
without a year, and episodes without a season and episode number (or, for daily shows, an air
date). Missing episode titles don't count, since sources often fill them in later and `reel sync`
picks them up. It lists each item with what it lacks and asks first, like `reel remove`, and takes
the same `--delete` and `--yes` flags; a query narrows it to part of the library.

Handled along the way:

- Plex `{tmdb-…}`/`{tvdb-…}` tags already present in names are used for a direct lookup.
- Editions (`{edition-Director's Cut}`) and split films (`- pt1`).
- Multiple versions of the same film or episode gain a resolution suffix (`- 1080p`). The copy
  already in the library is renamed the next time `reel move` runs. Copies that would share a
  name get a `copy 2`, `copy 3`, … suffix after the resolution.
- Multi-episode files (`s02e01-e03`), specials (`Season 00`) and date-based shows.
- Sidecar subtitles are renamed with their video, keeping suffixes such as `.en.forced.srt`.
- Samples and Plex extras folders (`Featurettes`, `Trailers`, …) are ignored.

### Queries

Queries follow beets: bare words match film, show and episode titles; `field:value` matches a
field; terms are ANDed; `^` negates a term.

```sh
reel ls kind:movie year:1990..1999
reel ls hdr:dv dv_profile:fel            # Dolby Vision profile 7 FEL
reel ls hdr:hdr10+ ^atmos:true           # HDR10+ without any Atmos track
reel ls audio_codec:truehd audio_channels:8
reel ls subtitle_language:eng subtitle_forced:true
reel ls -f '$title ($year) $resolution $hdr $audio_codec' doctor season:1
```

Text fields match substrings, or exactly with `field:=value`. Numeric fields accept `n`, `a..b`,
`a..` and `..b`. `hdr` matches one format out of the stored list (`DV`, `HDR10+`, `HDR10`, `HLG`,
or `SDR`). Audio and subtitle fields match if any stream of the file matches.

### Media information

Recorded per file: resolution, video codec, bit depth, colour space, primaries, transfer function,
HDR formats and Dolby Vision profile (`5`, `8.1`, `8.4`, `7 FEL`, `7 MEL`, …). Recorded per audio
stream: title, language, codec and profile, channel count and layout, and Dolby Atmos presence.
Recorded per embedded subtitle stream: title, language, codec, and forced/SDH flags.

HDR10+ and the profile 7 enhancement-layer type are read from the first video frame's metadata.
The FEL/MEL distinction is a heuristic on the RPU's non-linear quantisation parameters.

## Design

| Module | Role |
| --- | --- |
| `parse` | File name → title, year, episode numbering, edition, part |
| `metadata` | `MovieSource`/`ShowSource` traits, TMDB and TVDB clients, candidate scoring |
| `import` | Discovery, grouping, match decisions, and the per-file import step |
| `naming` | Plex-convention paths, sidecar subtitle discovery |
| `probe` | ffprobe invocation and interpretation |
| `library`, `query` | SQLite storage and query compilation |
| `commands` | list, info, move, update, remove, purge, stats |
| `sync` | Refreshing identified items from their metadata source |
| `missing` | Listing episodes of shows that have no file |
| `unmatched` | Listing library files that are not in the database |

## Potential future work

- Fetching sidecar subtitles from OpenSubtitles.
- Integration with [videoconverter](https://github.com/Sciencentistguy/videoconverter).
- Plex local extras (trailers, featurettes) rather than ignoring them.
