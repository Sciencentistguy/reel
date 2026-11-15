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

## Setup

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

### Running from a checkout

For testing without installing or touching your real library, keep a throwaway configuration in
the git-ignored `dev/` directory. Relative paths in a configuration file are resolved against the
file's own directory, so everything stays inside `dev/`:

```toml
# dev/config.toml
[library]
database = "library.db"
movies = "Movies"
tv = "TV Shows"

[metadata]
tmdb_api_key = "..."
tvdb_api_key = "..."
```

```sh
export REEL_CONFIG=$PWD/dev/config.toml   # or pass -c dev/config.toml each time
cargo run -- import -n ~/Downloads/some-show
nix run . -- stats                        # the same, built by Nix
```

## Commands

| Command | Purpose |
| --- | --- |
| `reel import PATH...` | Identify files and file them into the library |
| `reel list [QUERY]` | List items (`-f` for a template, `-p` for paths) |
| `reel info [QUERY]` | Show everything known about items, including streams |
| `reel move [QUERY]` | Rename files to match the current naming scheme |
| `reel update [QUERY]` | Re-probe changed files, drop entries whose files have gone |
| `reel remove QUERY` | Remove items from the library (`-d` also deletes the files) |
| `reel stats [QUERY]` | Summarise the library |
| `reel fields` | List fields usable in queries and templates |
| `reel config` | Show the effective configuration |

### Importing

`reel import` walks the given paths, guesses a title, year and episode numbering from each file
name (falling back to parent directories), and groups files by film or show. Each group is looked
up once. A confident match is applied automatically; otherwise you choose from the candidates,
search again, enter an ID, use the names as they are, or skip. `--quiet` never prompts and skips
anything uncertain. `--asis` skips lookups altogether, and `--pretend` shows what would happen.

Handled along the way:

- Plex `{tmdb-…}`/`{tvdb-…}` tags already present in names are used for a direct lookup.
- Editions (`{edition-Director's Cut}`) and split films (`- pt1`).
- Multiple versions of the same film or episode gain a resolution suffix (`- 1080p`). The copy
  already in the library is renamed the next time `reel move` runs.
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
| `commands` | list, info, move, update, remove, stats |

## Future work

- Fetching sidecar subtitles from OpenSubtitles.
- Integration with [videoconverter](https://github.com/Sciencentistguy/videoconverter).
- Plex local extras (trailers, featurettes) rather than ignoring them.
