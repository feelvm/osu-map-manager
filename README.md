# osu! Map Manager

A desktop tool for creating and maintaining osu! collections from the beatmaps
already installed on your Windows PC. Scan your `Songs` folder, filter maps by
artist, title, mapper, star rating, length, mode and more, then write the
results into osu!'s `collection.db`.

> Looking for per-tab details, safety rules, caches and edge cases? See
> [TECHNICAL.md](TECHNICAL.md).

## Features

- Find maps in your local library with osu!-style filters, and preview map backgrounds and audio (via FFmpeg).
- Build, inspect and edit collections, and save them to `collection.db` (with automatic backups).
- Export a TSV list of selected maps.
- Detect maps with missing audio or backgrounds and repair the affected beatmapsets — presets you deleted on purpose can be ignored.
- Update outdated maps from the online versions, and delete natively mapped taiko/catch/mania `.osu` files you don't want.
- Shrink Beatmap Assets: compress audio, videos, images and skins to save disk, with backups and undo.
- Skin Editor: preview installed skins and remix their elements.
- Backgrounds: set one image as the background of every beatmap without touching `.osu`/`.osb` files (undo included).
- Update the app itself from GitHub Releases.

## Requirements

- [osu!](https://osu.ppy.sh/home/download) with a local `Songs` folder
- [FFmpeg](https://ffmpeg.org/download.html) on `PATH`, in `FFMPEG_PATH`, or beside the app executable

## Install

**Download a release:** grab the latest build from
[Releases](https://github.com/feelvm/osu-map-manager/releases), unzip, and run
`osu-map-manager.exe`.

**Build from source:** requires
[Rustup](https://doc.rust-lang.org/cargo/getting-started/installation.html):

```powershell
cargo run --release
```

## Usage

The app is organized into tabs: `Library`, `Collections`, `Fix maps`, `Shrink`,
`Skin Editor`, and `Backgrounds`.

1. Click `Load my maps` to read your installed beatmaps.
2. Add filters to find the maps you want.
3. Select the maps you want in the collection.
4. Choose a collection name and click `Apply`.
