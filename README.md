# osu! Map Manager

osu! Map Manager is a desktop tool for creating and maintaining osu! collections from the
beatmaps already installed on your Windows computer.

Use it to scan your osu! `Songs` folder, filter maps by artist, title, mapper, difficulty values,
star rating, length, mode, and other fields, then write the selected results into osu!'s
`collection.db`.

## What You Can Do

- Find maps in your local osu! library with osu!-style filters.
- Preview map backgrounds and play map audio inside the app through FFmpeg.
- Build a collection from the matching maps.
- Inspect and edit existing collections from `collection.db`.
- Save that collection directly to `collection.db`.
- Export a TSV list of selected maps for review.
- Detect maps with missing audio or background files.
- Repair affected beatmapsets.
- Delete natively mapped taiko, catch, or mania `.osu` files you do not want.
- Preview installed skins and remix their elements in the Skin Editor.
- Update the app itself from GitHub Releases.

## Before You Start

You need:

- [osu!](https://osu.ppy.sh/home/download) installed on your computer.
- A local osu! `Songs` folder with beatmaps in it.
- [FFmpeg](https://ffmpeg.org/download.html) available on `PATH`, set through `FFMPEG_PATH`, or placed beside the app executable.

## Installing the App

### Option 1: Download a Release

Download the latest Windows build from
[GitHub Releases](https://github.com/feelvm/osu-map-manager/releases), unzip it, and run
`osu-map-manager.exe`. No installation steps are needed.

### Option 2: Build from Source

Building requires [Rustup](https://doc.rust-lang.org/cargo/getting-started/installation.html).
From the repository folder:

```powershell
cargo run --release
```

This compiles the app and starts it. `cargo build --release` produces the standalone
executable under `target\release\` without starting it.

## Basic Workflow

The app is organized into tabs: `Library` (scan, filter and pick maps),
`Collections` (build and save collections), `Maintenance` (repair, update and
clean up), `Shrink` (compress audio, video and backgrounds), and `Skin Editor`
(preview, remix and save skins).

1. Open osu! Map Manager.
2. Confirm the auto-detected osu! `Songs` folder.
3. Click scan to read your installed beatmaps.
4. Add one or more filters.
5. Review the matching maps.
6. Select the maps you want in the collection.
7. Choose a collection name.
8. Write `collection.db`.

The app finds your `Songs` folder automatically (the standard install location under
`%LOCALAPPDATA%\osu!`) and derives the osu! install root from it, so it can also read
`osu!.db` and `collection.db` next to `Songs` when they exist.

The app backs up your existing `collection.db` before replacing it, keeping the
last three versions (`collection.db.bak`, `.bak.1`, `.bak.2`).

After a full scan, the app caches the parsed library under `.osu-map-manager`. On the next start it
loads that cache immediately, and the next scan reuses cached maps while parsing newly added `.osu`
files.

## Filtering Maps

Filters are combined together. A map must match every active filter to appear in the result list.

- Words: type an artist, title, mapper, difficulty name, or user tag. Empty means anything.
- Difficulty: tick Stars, AR, CS, OD, HP, or BPM and drag the min/max sliders.
- Song: type a length range in seconds and pick a game mode from the dropdown.

The panel also shows the equivalent osu! search text, for example
`artist=Camellia stars>=5.5 mode=osu`.

Some filters shown in the app depend on metadata that may not exist in local `.osu` files. If a
filter such as ranked status or favourite count does not behave as expected, the map data likely
is not available locally yet.

## Writing a Collection

When you write `collection.db`, osu! Map Manager creates or updates a collection with your chosen
name and the selected maps.

You can also load existing collections from `collection.db`, inspect their stored beatmap hashes,
load one into the current selection, then add or remove scanned maps before saving it back. Editing
the name of a loaded collection before saving renames that collection.

Before writing:

- Close osu! if it is open.
- Make sure the selected maps are the maps you want.
- Keep the backup file until you have confirmed the collection appears correctly in osu!.

After writing, start osu! and check the Collections tab.

## Repairing Maps

The app can detect installed maps that reference missing required files, such as missing audio or
background files.

Open the `Maintenance` tab to fix them. Each affected beatmapset lists its missing files
and has its own `Repair this set` button (or use `Repair all`). Repair redownloads the
beatmapset through the built-in backend and restores only the missing files,
so your local scores and edits are left untouched. The log reports where each download came from
and which files were restored; repaired folders are rescanned automatically afterwards.

For downloads via the official osu! API, click `Sign in with osu!`. This opens osu! in your
browser and completes through the backend Worker (which holds the OAuth client secret), then the
app downloads with your user token. Without sign-in, downloads fall back to the mirror.

Your osu! OAuth app must have its callback URL set to exactly
`http://127.0.0.1:3000/callback`, otherwise osu! answers the sign-in page with
401 `invalid_client`.

## Updating Outdated Maps

osu! marks maps with `update to latest version` when the installed `.osu` file no longer matches
the online version. The `Maintenance` tab has an `Update outdated beatmaps` section that
does the same comparison in bulk:

1. Click `Check for updates`. The app compares every installed difficulty that has an online
   beatmap id against the current osu!web checksums (the same signal osu! uses). Checking works
   without sign-in, but is paced slower then to respect osu!'s rate limits; signing in spends
   your own quota instead of the shared one and checks faster.
2. Review the outdated sets — each lists which difficulties changed and the online version date.
3. Click `Update all` or update a single set. The app redownloads the beatmapset (official osu!
   API when signed in, mirror otherwise), overwrites it with the latest files, removes
   difficulties deleted upstream, verifies the result against the online checksums, then
   rescans the updated files automatically.

Sets that no longer exist online are reported and skipped. Difficulties without an online
beatmap id cannot be checked.

## Cleaning up Non-Standard Modes

The `Maintenance` tab has a `Clean up non-std modes` section that removes natively mapped
taiko, catch, and mania `.osu` files from your library. Tick the modes you want gone, then
click `Delete selected non-std maps` and confirm.

Converted maps share the original std `.osu` file, so only natively mapped taiko/catch/mania
files can appear here — the original std difficulties are never touched. Deletion is
permanent: the files are removed from disk (not moved to the Recycle Bin), so make sure the
selection matches what you want before confirming.

## Editing Skins

The `Skin Editor` tab lists the skins found in your `<osu root>/Skins` folder and previews
each one with a mock gameplay view drawn from the skin's own elements — hit circles with the
`skin.ini` combo colours, approach circles, combo numbers and a moving cursor, honouring the
`skin.ini` switches that visibly change circles and the cursor.

From there you can:

- Replace any element with an asset pooled from every installed skin, and import your own
  image files straight into the matching element slots.
- Scale the cursor with a single `Cursor size` control that keeps each file's exact pixel
  canvas.
- Save the result as a complete copy of the base skin named `<skin name> v1` (then `v2`, and
  so on, skipping versions that already exist). The base skin is never modified.

## Updating the App

The `↻ Update` button in the sidebar opens an `App update` window that checks GitHub Releases
for a newer version. If one is available, `Download & restart` fetches it, replaces the app
executable, and restarts the app automatically. Checking is manual; nothing happens in the
background.

## Exporting a Map List

Use TSV export when you want a plain-text list of the selected maps before writing a collection.

## Shrinking Beatmap Assets

The `Shrink` tab compresses song audio, background video/images and skins to save disk:

1. Scan your library, then open `Shrink` and click `Analyze library` (and optionally
   `Analyze skins` for `<osu root>/Skins`).
2. Review the totals; analysis and shrinking can be stopped or paused at any time.
3. Click `Shrink N set(s)` to process everything analyzed.

Rules that keep the library safe:

- Only file *contents* ever change: filenames and `.osu`/`.osb` files are never
  touched, so maps keep submitting scores. Close osu! first.
- Song audio goes to 192k MP3 / OGG q6, video to H.264 ≤720p without audio,
  images are downscaled past 1920px with a quality pass. Files that would grow
  are kept untouched instead of failing.
- Storyboard `Sample` sounds count as protected references (never orphans).
- Already-shrunk files are remembered in `.osu-map-manager/shrink_cache.json`
  and skipped on later runs — no wasted re-encodes, no stacked JPEG generations.
  Changing quality settings re-plans affected files; `Clear shrink cache` forgets all.
- `.wav` hitsounds, `.osu`/`.osb`/`skin.ini` and animated `.gif` are never touched.
- Anything convertible only via a rename (`.flv` video, `.wav` song audio) is
  skipped outright rather than risking checksum changes.
- Each folder is zipped under `.osu-map-manager/shrink-backups` first; restore any
  backup from the tab if something looks wrong in-game.
- Optional extras, both default off: delete media nothing references, and remove
  background videos entirely (the game shows the background image instead).
- Skins get a pixels-exact pass only (same names, same dimensions, tighter encodes);
  `skin.ini` and skin sounds are left alone.

## Notes and Limitations

- The app scans local files; it does not automatically know everything shown on osu!web.
- Some osu!web-only fields need API-backed metadata before they can be matched reliably.
- Close osu! before replacing `collection.db`.
- Keep backups until you confirm the result in osu!.

## References

- [osu! beatmap search wiki](https://osu.ppy.sh/wiki/en/Beatmap_search)
