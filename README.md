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

## Before You Start

You need:

- [osu!](https://osu.ppy.sh/home/download) installed on your computer.
- A local osu! `Songs` folder with beatmaps in it.
- This app built or downloaded for your system.
- [FFmpeg](https://ffmpeg.org/download.html) available on `PATH`, set through `FFMPEG_PATH`, or placed beside the app executable.
- [Rustup](https://doc.rust-lang.org/cargo/getting-started/installation.html) installed for building from source.

If you are running from source, start it with:

```powershell
cargo run
```

## Basic Workflow

1. Open osu! Map Manager.
2. Enter your osu! `Songs` folder.
3. Click scan to read your installed beatmaps.
4. Add one or more filters.
5. Review the matching maps.
6. Select the maps you want in the collection.
7. Choose a collection name.
8. Write `collection.db`.

The app backs up your existing `collection.db` before replacing it.

## Choosing the Songs Folder

Enter your osu! `Songs` folder, for example:

```text
C:\Users\%USERPROFILE%\AppData\Local\osu!\Songs
```

The app derives the osu! install root from it automatically, so it can also read
`osu!.db` and `collection.db` next to `Songs` when they exist:

```text
Songs\
osu!.db
```

The `Songs` folder contains the `.osu` files. `osu!.db` can provide extra local metadata such as
stored star ratings where available.

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

Open the `Repairs and delete` tab to fix them. Each affected beatmapset lists its missing files
and has its own `Repair this set` button (or use `Repair all`). Repair redownloads the
beatmapset through the built-in backend and restores only the missing files,
so your local scores and edits are left untouched. The log reports where each download came from
and which files were restored; rescan afterwards to confirm the issues are gone.

For downloads via the official osu! API, click `Sign in with osu!`. This opens osu! in your
browser and completes through the backend Worker (which holds the OAuth client secret), then the
app downloads with your user token. Without sign-in, downloads fall back to the mirror.

Your osu! OAuth app must have its callback URL set to exactly
`http://127.0.0.1:3000/callback`, otherwise osu! answers the sign-in page with
401 `invalid_client`.

## Updating Outdated Maps

osu! marks maps with `update to latest version` when the installed `.osu` file no longer matches
the online version. The `Repairs and delete` tab has an `Update outdated beatmaps` section that
does the same comparison in bulk:

1. Click `Check for updates`. The app compares every installed difficulty that has an online
   beatmap id against the current osu!web checksums (the same signal osu! uses). Checking works
   without sign-in.
2. Review the outdated sets — each lists which difficulties changed and the online version date.
3. Click `Update all` or update a single set. The app redownloads the beatmapset (official osu!
   API when signed in, mirror otherwise), overwrites it with the latest files, removes
   difficulties deleted upstream, verifies the result against the online checksums, then
   rescans the updated files automatically.

Sets that no longer exist online are reported and skipped. Difficulties without an online
beatmap id cannot be checked.

## Exporting a Map List

Use TSV export when you want a plain-text list of the selected maps before writing a collection.

## Notes and Limitations

- The app scans local files; it does not automatically know everything shown on osu!web.
- Some osu!web-only fields need API-backed metadata before they can be matched reliably.
- Close osu! before replacing `collection.db`.
- Keep backups until you confirm the result in osu!.

## References

- [osu! beatmap search wiki](https://osu.ppy.sh/wiki/en/Beatmap_search)
