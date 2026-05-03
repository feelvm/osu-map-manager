# osu! Map Manager

osu! Map Manager is a desktop tool for creating and maintaining osu! collections from the
beatmaps already installed on your computer.

Use it to scan your osu! `Songs` folder, filter maps by artist, title, mapper, difficulty values,
star rating, length, mode, and other fields, then write the selected results into osu!'s
`collection.db`.

## What You Can Do

- Find maps in your local osu! library with osu!-style filters.
- Build a collection from the matching maps.
- Save that collection directly to `collection.db`.
- Export a TSV list of selected maps for review.
- Detect maps with missing audio or background files.
- Repair or update affected beatmapsets when a download backend is configured.

## Before You Start

You need:

- osu! installed on your computer.
- A local osu! `Songs` folder with beatmaps in it.
- This app built or downloaded for your system.

If you are running from source, start it with:

```powershell
cargo run
```

## Basic Workflow

1. Open osu! Map Manager.
2. Select your osu! folder, or select the `Songs` folder directly.
3. Click scan to read your installed beatmaps.
4. Add one or more filters.
5. Review the matching maps.
6. Select the maps you want in the collection.
7. Choose a collection name.
8. Write `collection.db`.

The app backs up your existing `collection.db` before replacing it.

## Choosing the osu! Folder

The app works best when you select the main osu! folder, not only `Songs`.

For example:

```text
C:\Users\%USERPROFILE%\AppData\Local\osu!
```

That lets the app read both:

```text
Songs\
osu!.db
```

The `Songs` folder contains the `.osu` files. `osu!.db` can provide extra local metadata such as
stored star ratings where available.

## Filtering Maps

Filters are combined together. A map must match every active filter to appear in the result list.

Useful filters include:

- artist
- title
- mapper
- difficulty name
- source
- tag
- stars
- AR, CS, OD, HP
- BPM
- length
- circles, sliders, keys
- mode

Example searches:

```text
artist=Camellia stars>=5.5
creator=Natteke length<120
title=night ar>=9 mode=osu
```

Some filters shown in the app depend on metadata that may not exist in local `.osu` files. If a
filter such as ranked status or favourite count does not behave as expected, the map data likely
is not available locally yet.

## Writing a Collection

When you write `collection.db`, osu! Map Manager creates or updates a collection with your chosen
name and the selected maps.

Before writing:

- Close osu! if it is open.
- Make sure the selected maps are the maps you want.
- Keep the backup file until you have confirmed the collection appears correctly in osu!.

After writing, start osu! and check the Collections tab.

## Repairing Maps

The app can detect installed maps that reference missing required files, such as missing audio or
background files.

If repair downloads are configured, you can use the repair workflow to redownload affected
beatmapsets and restore missing files.

## Updating Maps

The update workflow checks whether installed beatmapsets have newer metadata or downloads
available through the configured backend.

As with repair, update downloads require a backend URL. Without one, local scanning and collection
creation still work.

## Exporting a Map List

Use TSV export when you want a plain-text list of the selected maps before writing a collection.

This is useful for:

- reviewing large result sets
- comparing filter changes
- keeping a record of what was selected

## Notes and Limitations

- The app scans local files; it does not automatically know everything shown on osu!web.
- Some osu!web-only fields need API-backed metadata before they can be matched reliably.
- Close osu! before replacing `collection.db`.
- Keep backups until you confirm the result in osu!.

## References

- [osu! beatmap search wiki](https://osu.ppy.sh/wiki/en/Beatmap_search)
