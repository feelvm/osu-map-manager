//! Skin Editor tab.
//!
//! Lists the skins found in `<osu root>/Skins`, previews each one with a mock
//! gameplay "screenshot" drawn from its own elements, and lets the user remix
//! elements from the pooled assets of every installed skin. Saving writes a
//! complete copy of the base skin as `<skin name> v<N>` (`v1`, then `v2`, …
//! skipping versions that already exist), with every picked pool asset copied
//! in under its canonical element name.
//!
//! The cursor additionally gets a "Cursor size" control in the Skin extras
//! panel: one scale for all cursor assets (cursor, trail, middle), scaled
//! together inside each file's original canvas resolution, so the files keep
//! their exact pixel size (which is what the game sizes elements by) while
//! the art within them — and with it the in-game appearance — shrinks or
//! grows.
//!
//! Own assets can be imported straight into the editor: picked files (or a
//! whole folder) are filed into the element slots their file names match —
//! the same stem/`@2x` matching the skin scan uses — pooled as "Imported"
//! tiles in the right category and set as the active pick, so the preview
//! and the next save pick them up.
//!
//! The gameplay preview is drawn with the egui painter on a black backdrop
//! (no backgrounds — 100% dim): hit circles with approach circle and combo
//! numbers, plus a moving cursor. The HUD (health bar, score, accuracy and
//! combo readouts) is currently paused and hidden from the preview and the
//! options — flip `HUD_ENABLED` to bring it back. Hit circles and approach
//! circles are tinted with the skin.ini `[Colours]` combo colours, so
//! switching skins or picking pooled assets updates the mock faithfully.
//!
//! The preview also honours the skin.ini switches that visibly change circles
//! and cursor: `HitCircleOverlayAboveNumber` (legacy typo `…Numer` accepted)
//! flips the overlay/number draw order, `[Fonts] HitCirclePrefix` locates
//! digits kept in subfolders (WhiteCat's `Assets/default/default-N.png`), and
//! `CursorCentre` / `CursorExpand` / `CursorRotate` / `CursorTrailRotate` drive
//! the cursor origin, click swell and spin. Slider, spinner, comboburst and
//! multi-digit overlap settings have nothing to act on in this scene and are
//! ignored.
//!
//! Skins frequently ship `@2x`-only elements whose artwork sits at 1x scale
//! inside a twice-as-large canvas. Decodes are therefore cropped to their
//! opaque bounds before display, which keeps every skin's art at the same
//! visual size in previews.
//!
//! Circle layers are drawn at game scale — each layer's trimmed 1x size
//! against the 128-unit reference circle — so multi-layer circles keep the
//! relative sizes the skin authored (e.g. a 102-unit fill inside a 118-unit
//! ring keeps its visible inner edge, exactly like in game) instead of being
//! stretched to fill one shared box.
//!
//! Instafade-style skins go one step further: their `hitcircle` and
//! `hitcircleoverlay` files are fully transparent, and the circle art is baked
//! into the `default-N` combo-number sprites instead. Those numbers already
//! draw at game scale (plus the game's 0.8x number downscale), so the baked-in
//! circle lands where the game puts it instead of shrinking to a dot.

use crate::{app::muted_label, app::section_frame};
use anyhow::{Context as _, Result};
use eframe::egui;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::Duration,
};

// ── Element slot table ───────────────────────────────────────────────────────

/// One editable skin element, addressed by a unique key. A slot covers either
/// a single canonical file or a bundled set (e.g. all ten digits) that is
/// picked, previewed and replaced as one unit.
struct Slot {
    key: &'static str,
    label: &'static str,
    files: &'static [&'static str],
    /// Member whose thumbnail represents the slot (must be in `files`).
    preview_file: &'static str,
}

/// Single-file slot: one canonical file, picked and shown on its own.
macro_rules! single {
    ($key:literal, $label:literal, $file:literal) => {
        Slot {
            key: $key,
            label: $label,
            files: &[$file],
            preview_file: $file,
        }
    };
}

/// The slots that make up the whole cursor. The Skin extras "Cursor size"
/// control scales the assets of all of them together (see
/// `scale_within_canvas`).
const CURSOR_SLOT_KEYS: &[&str] = &["cursor", "cursortrail", "cursormiddle"];

struct Group {
    label: &'static str,
    default_open: bool,
    /// HUD-backed group (health bar / score-combo-accuracy digits): hidden
    /// from the options and the preview while the HUD is paused.
    hud: bool,
    slots: &'static [Slot],
}

/// The HUD (health bar, score, accuracy and combo readouts) is paused for now
/// — the focus is on circles and cursor. Flip to `true` to bring the HUD back
/// into the preview and the editor options at their saved sizes.
const HUD_ENABLED: bool = false;

/// Groups currently shown in the editor and fed into the scan/pool.
fn active_groups() -> impl Iterator<Item = &'static Group> {
    GROUPS.iter().filter(|group| HUD_ENABLED || !group.hud)
}

static CIRCLE_NUMBERS: &[&str] = &[
    "default-0.png",
    "default-1.png",
    "default-2.png",
    "default-3.png",
    "default-4.png",
    "default-5.png",
    "default-6.png",
    "default-7.png",
    "default-8.png",
    "default-9.png",
];

static SCORE_DIGITS: &[&str] = &[
    "score-0.png",
    "score-1.png",
    "score-2.png",
    "score-3.png",
    "score-4.png",
    "score-5.png",
    "score-6.png",
    "score-7.png",
    "score-8.png",
    "score-9.png",
];

static GROUPS: &[Group] = &[
    Group {
        label: "Cursor",
        default_open: true,
        hud: false,
        slots: &[
            single!("cursor", "Cursor", "cursor.png"),
            single!("cursortrail", "Cursor trail", "cursortrail.png"),
            single!("cursormiddle", "Cursor middle", "cursormiddle.png"),
        ],
    },
    Group {
        label: "Hit circles",
        default_open: true,
        hud: false,
        slots: &[
            single!("hitcircle", "Hit circle", "hitcircle.png"),
            single!(
                "hitcircleoverlay",
                "Hit circle overlay",
                "hitcircleoverlay.png"
            ),
            single!("approachcircle", "Approach circle", "approachcircle.png"),
            single!(
                "hitcircleselect",
                "Hit circle select (editor)",
                "hitcircleselect.png"
            ),
        ],
    },
    Group {
        label: "Health bar",
        default_open: false,
        hud: true,
        slots: &[
            single!("scorebar-bg", "Bar background", "scorebar-bg.png"),
            single!("scorebar-colour", "Bar fill", "scorebar-colour.png"),
            single!("scorebar-marker", "Bar marker", "scorebar-marker.png"),
        ],
    },
    Group {
        label: "Circle numbers",
        default_open: false,
        hud: false,
        slots: &[Slot {
            key: "circle-numbers",
            label: "Numbers (0–9)",
            files: CIRCLE_NUMBERS,
            preview_file: "default-2.png",
        }],
    },
    Group {
        label: "Score, combo & accuracy digits",
        default_open: false,
        hud: true,
        slots: &[
            Slot {
                key: "score-digits",
                label: "Digits (0–9)",
                files: SCORE_DIGITS,
                preview_file: "score-2.png",
            },
            single!("score-x", "Combo “x”", "score-x.png"),
            single!("score-comma", "Comma", "score-comma.png"),
            single!("score-percent", "Percent", "score-percent.png"),
        ],
    },
    Group {
        label: "Text & banners",
        default_open: false,
        hud: false,
        slots: &[
            single!("ready", "Countdown “READY?”", "ready.png"),
            single!("go", "Countdown “GO!”", "go.png"),
            single!("sectionpass", "Section pass", "sectionpass.png"),
            single!("sectionfail", "Section fail", "sectionfail.png"),
            single!("play-skip", "Skip button", "play-skip.png"),
            single!("comboburst", "Combo burst", "comboburst.png"),
        ],
    },
];

fn total_file_count() -> usize {
    active_groups()
        .flat_map(|group| group.slots)
        .map(|slot| slot.files.len())
        .sum()
}

/// `cursor.png` → `cursor`.
fn slot_stem(slot_file: &str) -> &str {
    slot_file
        .split_once('.')
        .map_or(slot_file, |(stem, _)| stem)
}

/// Does a folder entry pool into this slot file? Matches the canonical stem,
/// both `@2x` and plain, in any of the containers the game loads elements from.
fn file_matches_slot(file_name: &str, slot_file: &str) -> bool {
    let lower = file_name.to_ascii_lowercase();
    let (stem, ext) = match lower.rsplit_once('.') {
        Some((stem, ext)) => (stem, ext),
        None => return false,
    };
    let stem = stem.strip_suffix("@2x").unwrap_or(stem);
    stem == slot_stem(slot_file) && matches!(ext, "png" | "jpg" | "jpeg")
}

// ── Skin scanning & asset pooling ────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SkinSummary {
    pub folder_name: String,
    pub path: PathBuf,
    /// `Name:` from skin.ini, falling back to the folder name.
    pub display_name: String,
    pub author: Option<String>,
    /// `[Colours] Combo1/2/…` values in numeric order (Combo1 at index 0,
    /// commented-out lines are ignored) — the preview tints hit circles and
    /// approach circles with these like in game (first combo = Combo2, the
    /// list wrapping back to Combo1 last).
    pub combo_colours: Vec<egui::Color32>,
    /// Circle/cursor behaviour switches from skin.ini that change how the
    /// preview must draw (overlay order, cursor centre / expand / spin, …).
    pub render_opts: SkinRenderOpts,
    pub file_count: usize,
    /// canonical element file → actual file in this skin (`@2x` preferred).
    pub elements: BTreeMap<&'static str, PathBuf>,
    /// Root-level `failsound` audio files (`failsound.mp3`/`.ogg`/`.wav`,
    /// any case) — what the game plays on fail. Collected during the scan so
    /// the editor can offer to hide them.
    pub failsound: Vec<PathBuf>,
    /// Hidden `failsound` audio files (`failsound.mp3.bak`, …) — ignored by
    /// the game, restorable from the editor.
    pub failsound_hidden: Vec<PathBuf>,
}

impl SkinSummary {
    pub fn element_count(&self) -> usize {
        self.elements.len()
    }
}

#[derive(Debug, Clone)]
pub struct PoolEntry {
    /// Skin folder the assets came from (representative when several skins
    /// share identical content — see `also_in`).
    pub skin: String,
    /// member canonical file → actual file in that skin (existing members only).
    pub files: BTreeMap<&'static str, PathBuf>,
    /// Representative file shown in the picker thumbnail.
    pub thumb: PathBuf,
    /// Other skin folders with pixel-identical content for this slot,
    /// collapsed into this single tile so the pool doesn't repeat.
    /// Sorted, never contains `skin` itself.
    pub also_in: Vec<String>,
    /// Every member image is fully transparent (e.g. placeholder
    /// `cursortrail.png` skins ship to disable the trail). Blanks from all
    /// skins collapse into one tile and sort last.
    pub is_blank: bool,
}

struct SkinMeta {
    name: Option<String>,
    author: Option<String>,
    combo_colours: Vec<egui::Color32>,
    render_opts: SkinRenderOpts,
    hitcircle_prefix: Option<String>,
}

/// Circle/cursor behaviour switches from skin.ini `[General]` that visibly
/// change gameplay rendering, mirrored here so the preview shows what the
/// game shows. Defaults match the wiki (and lazer's `LegacyCursor`):
/// overlay above numbers, centred cursor, expand + spin on, trail spin off.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SkinRenderOpts {
    /// `HitCircleOverlayAboveNumber` (legacy typo `HitCircleOverlayAboveNumer`
    /// still honoured): draw the overlay above the combo number.
    pub overlay_above_number: bool,
    /// `CursorCentre`: cursor origin at the image centre (`false` = top-left).
    pub cursor_centre: bool,
    /// `CursorExpand`: cursor swells when clicking.
    pub cursor_expand: bool,
    /// `CursorRotate`: cursor spins (one clockwise revolution per 10 s).
    pub cursor_rotate: bool,
    /// `CursorTrailRotate`: trail ghosts spin with the cursor.
    pub cursortrail_rotate: bool,
}

impl Default for SkinRenderOpts {
    fn default() -> Self {
        Self {
            overlay_above_number: true,
            cursor_centre: true,
            cursor_expand: true,
            cursor_rotate: true,
            cursortrail_rotate: false,
        }
    }
}

/// Parses a skin.ini boolean: the game only recognises `1` as true and `0`
/// as false (trailing `//` comments ignored). Anything else is not a value.
fn parse_ini_bool(value: &str) -> Option<bool> {
    match value.split("//").next().unwrap_or(value).trim() {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    }
}

/// Sanitises a `[Fonts]` path prefix (`Assets/default/default`): forward
/// slashes, no leading `./` or `/`, and never escaping the skin folder.
fn sanitise_prefix(raw: &str) -> Option<String> {
    let mut prefix = raw
        .split("//")
        .next()
        .unwrap_or(raw)
        .trim()
        .replace('\\', "/");
    while prefix.starts_with("./") {
        prefix = prefix[2..].to_owned();
    }
    prefix = prefix
        .trim_start_matches('/')
        .trim_end_matches('/')
        .to_owned();
    if prefix.is_empty()
        || prefix
            .split('/')
            .any(|part| part.is_empty() || part == ".." || part == ".")
        || prefix.contains(':')
    {
        return None;
    }
    Some(prefix)
}

/// Reads a skin.ini as text, tolerating the encodings these files appear in:
/// UTF-16 LE/BE (BOM), UTF-8 with BOM, and anything else lossily mapped to
/// UTF-8 (the ASCII keys survive, so parsing still works).
fn read_skin_text(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let units: Vec<u16> = bytes[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|chunk| u16::from_le_bytes(*chunk))
            .collect();
        return Some(String::from_utf16_lossy(&units));
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        let units: Vec<u16> = bytes[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|chunk| u16::from_be_bytes(*chunk))
            .collect();
        return Some(String::from_utf16_lossy(&units));
    }
    let text = String::from_utf8_lossy(&bytes);
    Some(text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned())
}

/// Reads `Name:`, `Author:`, the `[Colours]` combo colours, the circle/cursor
/// `[General]` switches and the `[Fonts] HitCirclePrefix` out of a skin.ini.
/// Lines commented out with `//` (or `;`) are skipped. Combo colours are
/// stored in `ComboN` numeric order (Combo1 at index 0, Combo2 at index 1, …)
/// regardless of file order; keys without a numeric suffix are ignored.
fn read_skin_meta(path: &Path) -> SkinMeta {
    let mut meta = SkinMeta {
        name: None,
        author: None,
        combo_colours: Vec::new(),
        render_opts: SkinRenderOpts::default(),
        hitcircle_prefix: None,
    };
    let Some(text) = read_skin_text(path) else {
        return meta;
    };
    let mut section = String::new();
    let mut combos: BTreeMap<u32, egui::Color32> = BTreeMap::new();
    // Both overlay-order spellings are honoured like in game (lazer even
    // keeps the typo in its config enum for legacy skins); the correctly
    // spelled key wins when both are present.
    let mut overlay_above: Option<bool> = None;
    let mut overlay_above_typo: Option<bool> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("//") || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].to_ascii_lowercase();
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        if section == "general" {
            match key.as_str() {
                "name" if meta.name.is_none() => meta.name = Some(value.trim().to_owned()),
                "author" if meta.author.is_none() => {
                    meta.author = Some(value.trim().to_owned());
                }
                "cursorcentre" => {
                    if let Some(flag) = parse_ini_bool(value) {
                        meta.render_opts.cursor_centre = flag;
                    }
                }
                "cursorexpand" => {
                    if let Some(flag) = parse_ini_bool(value) {
                        meta.render_opts.cursor_expand = flag;
                    }
                }
                "cursorrotate" => {
                    if let Some(flag) = parse_ini_bool(value) {
                        meta.render_opts.cursor_rotate = flag;
                    }
                }
                "cursortrailrotate" => {
                    if let Some(flag) = parse_ini_bool(value) {
                        meta.render_opts.cursortrail_rotate = flag;
                    }
                }
                "hitcircleoverlayabovenumber" => {
                    if let Some(flag) = parse_ini_bool(value) {
                        overlay_above = Some(flag);
                    }
                }
                "hitcircleoverlayabovenumer" => {
                    if let Some(flag) = parse_ini_bool(value) {
                        overlay_above_typo = Some(flag);
                    }
                }
                _ => {}
            }
        } else if section == "fonts" {
            if key == "hitcircleprefix" && meta.hitcircle_prefix.is_none() {
                meta.hitcircle_prefix = sanitise_prefix(value);
            }
        } else if (section == "colours" || section == "colors")
            && let Some(number) = key
                .strip_prefix("combo")
                .and_then(|rest| rest.trim().parse::<u32>().ok().filter(|n| *n >= 1))
            && let Some(colour) = parse_colour(value)
        {
            combos.insert(number, colour);
        }
    }
    meta.combo_colours = combos.into_values().collect();
    meta.render_opts.overlay_above_number = overlay_above.or(overlay_above_typo).unwrap_or(true);
    meta
}

/// Parses `r, g, b` into a colour: 0–255 integers, 0–1 floats (scaled up), or
/// a mix; an alpha component and trailing inline comments are ignored.
fn parse_colour(value: &str) -> Option<egui::Color32> {
    let value = value.split("//").next().unwrap_or(value);
    let mut components = Vec::new();
    for part in value.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return None;
        }
        if let Ok(byte) = part.parse::<u8>() {
            components.push(f32::from(byte));
        } else if let Ok(float) = part.parse::<f32>()
            && float.is_finite()
        {
            components.push(if float <= 1.0 { float * 255.0 } else { float });
        } else {
            return None;
        }
    }
    if components.len() < 3 {
        return None;
    }
    let clamp = |value: f32| value.round().clamp(0.0, 255.0) as u8;
    Some(egui::Color32::from_rgb(
        clamp(components[0]),
        clamp(components[1]),
        clamp(components[2]),
    ))
}

/// Walks a skin folder's subdirectories for fallback element candidates.
/// Returns `(file name, full path, depth)` with depth 1 = direct child folder.
/// Dot-folders are skipped and recursion is capped so stray junk can't blow up
/// the scan. Root files are *not* included — callers try those first.
fn collect_nested_candidates(root: &Path) -> Vec<(String, PathBuf, usize)> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<(String, PathBuf, usize)>) {
        if depth > 6 {
            return;
        }
        let Ok(read_dir) = fs::read_dir(dir) else {
            return;
        };
        for entry in read_dir.filter_map(|entry| entry.ok()) {
            let path = entry.path();
            if path.is_dir() {
                if entry.file_name().to_string_lossy().starts_with('.') {
                    continue;
                }
                walk(&path, depth + 1, out);
            } else if path.is_file() {
                out.push((entry.file_name().to_string_lossy().to_string(), path, depth));
            }
        }
    }
    let mut out = Vec::new();
    // Depth 0 would be the root itself — start at 1 via its children.
    let Ok(read_dir) = fs::read_dir(root) else {
        return out;
    };
    for entry in read_dir.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        if path.is_dir() {
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            walk(&path, 1, &mut out);
        }
    }
    out
}

/// Best nested match for a slot file: shallowest folder wins, then `@2x`,
/// then `png` — mirroring the root preference.
fn pick_nested_slot<'a>(
    candidates: &'a [(String, PathBuf, usize)],
    slot_file: &str,
) -> Option<&'a PathBuf> {
    candidates
        .iter()
        .filter(|(name, _, _)| file_matches_slot(name, slot_file))
        .min_by_key(|(name, _, depth)| {
            let lower = name.to_ascii_lowercase();
            let is_2x = lower
                .rsplit_once('.')
                .is_some_and(|(stem, _)| stem.ends_with("@2x"));
            let is_png = lower.ends_with(".png");
            // `min_by_key`: shallower depth, then @2x, then png ranks first.
            (*depth, !is_2x, !is_png)
        })
        .map(|(_, path, _)| path)
}

/// Canonical `default-N.png` slot → the digit index, for `[Fonts]
/// HitCirclePrefix` resolution (the elements map stays keyed by canonical
/// name; only the file lookup follows the prefix).
fn circle_digit_index(slot_file: &str) -> Option<u8> {
    slot_file
        .strip_suffix(".png")
        .and_then(|stem| stem.strip_prefix("default-"))
        .and_then(|rest| rest.parse::<u8>().ok().filter(|digit| *digit <= 9))
}

/// Skin-root-relative path with forward slashes, lowercased for comparison.
fn skin_relpath(path: &Path, skin_root: &Path) -> Option<String> {
    path.strip_prefix(skin_root).ok().map(|relative| {
        relative
            .to_string_lossy()
            .replace('\\', "/")
            .to_ascii_lowercase()
    })
}

/// Resolve one combo digit through an explicit `[Fonts] HitCirclePrefix`
/// (e.g. WhiteCat's `Assets/default/default` → `Assets/default/default-2.png`).
/// Exact game paths only: `@2x` then plain, `png` before lossy containers.
/// Returns `None` when the prefix yields nothing so callers fall back to
/// plain stem matching.
fn pick_prefixed_digit(
    files: &[(String, PathBuf)],
    nested: &[(String, PathBuf, usize)],
    skin_root: &Path,
    prefix: &str,
    digit: u8,
) -> Option<PathBuf> {
    let lower_prefix = prefix.to_ascii_lowercase();
    let at_root = !prefix.contains('/');
    for ext in ["png", "jpg", "jpeg"] {
        for stem in [format!("{digit}@2x"), format!("{digit}")] {
            let want = format!("{lower_prefix}-{stem}.{ext}");
            if at_root
                && let Some((_, path)) = files
                    .iter()
                    .find(|(name, _)| name.to_ascii_lowercase() == want)
            {
                return Some(path.clone());
            }
            if let Some((_, path, _)) = nested
                .iter()
                .find(|(_, path, _)| skin_relpath(path, skin_root).as_deref() == Some(&want))
            {
                return Some(path.clone());
            }
        }
    }
    None
}

/// Scans one level of `Skins/` for skin folders. The game loads elements from
/// the skin root, so root files win; when a slot has no root match (e.g.
/// WhiteCat-style `assets/default/default-N.png` digits), descendants are
/// searched as a fallback so the preview still finds the art.
pub fn scan_skins(skins_dir: &Path) -> Vec<SkinSummary> {
    let mut skins = Vec::new();
    let Ok(entries) = fs::read_dir(skins_dir) else {
        return skins;
    };
    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let folder_name = entry.file_name().to_string_lossy().to_string();
        if folder_name.starts_with('.') {
            continue;
        }
        skins.push(scan_single_skin(&path, folder_name));
    }
    skins.sort_by_key(|skin| skin.folder_name.to_lowercase());
    skins
}

/// Scans one skin folder: its skin.ini, element files (root first, then
/// subfolder fallback) and failsound audio. Split out of [`scan_skins`] so
/// the cached scan can re-scan a single changed skin instead of everything.
fn scan_single_skin(skin_root: &Path, folder_name: String) -> SkinSummary {
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    if let Ok(read_dir) = fs::read_dir(skin_root) {
        for file in read_dir.filter_map(|file| file.ok()) {
            if file.path().is_file() {
                files.push((file.file_name().to_string_lossy().to_string(), file.path()));
            }
        }
    }
    let file_count = files.len();
    // Subfolder fallback (e.g. WhiteCat `assets/default/default-N.png`):
    // only walked when some slot misses at root.
    let mut nested_cache: Option<Vec<(String, PathBuf, usize)>> = None;

    let meta = files
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("skin.ini"))
        .map_or(
            SkinMeta {
                name: None,
                author: None,
                combo_colours: Vec::new(),
                render_opts: SkinRenderOpts::default(),
                hitcircle_prefix: None,
            },
            |(_, path)| read_skin_meta(path),
        );

    let mut elements = BTreeMap::new();
    for group in active_groups() {
        for slot in group.slots {
            for slot_file in slot.files {
                // An explicit `[Fonts] HitCirclePrefix` names the exact
                // game path first (WhiteCat keeps its digits under
                // `Assets/default/`); when it yields nothing, fall through
                // to plain stem matching below.
                let mut prefixed: Option<PathBuf> = None;
                if let (Some(prefix), Some(digit)) = (
                    meta.hitcircle_prefix.as_deref(),
                    circle_digit_index(slot_file),
                ) {
                    if nested_cache.is_none() {
                        nested_cache = Some(collect_nested_candidates(skin_root));
                    }
                    prefixed = pick_prefixed_digit(
                        &files,
                        nested_cache.as_ref().expect("nested cache just filled"),
                        skin_root,
                        prefix,
                        digit,
                    );
                }
                // The game prefers @2x; among equals prefer png — pick the
                // highest-scoring candidate. Root files win; only when the
                // root has no match do we fall back to subfolders.
                let pick = prefixed.or_else(|| {
                    files
                        .iter()
                        .filter(|(name, _)| file_matches_slot(name, slot_file))
                        .max_by_key(|(name, _)| {
                            let lower = name.to_ascii_lowercase();
                            let is_2x = lower
                                .rsplit_once('.')
                                .is_some_and(|(stem, _)| stem.ends_with("@2x"));
                            let is_png = lower.ends_with(".png");
                            (is_2x, is_png)
                        })
                        .map(|(_, path)| path.clone())
                        .or_else(|| {
                            if nested_cache.is_none() {
                                nested_cache = Some(collect_nested_candidates(skin_root));
                            }
                            nested_cache
                                .as_ref()
                                .and_then(|candidates| pick_nested_slot(candidates, slot_file))
                                .cloned()
                        })
                });
                if let Some(resolved) = pick {
                    elements.insert(*slot_file, resolved);
                }
            }
        }
    }

    let (failsound, failsound_hidden) = failsound_from_root(&files);
    SkinSummary {
        display_name: meta.name.unwrap_or_else(|| folder_name.clone()),
        folder_name,
        path: skin_root.to_path_buf(),
        author: meta.author,
        combo_colours: meta.combo_colours,
        render_opts: meta.render_opts,
        file_count,
        elements,
        failsound,
        failsound_hidden,
    }
}

/// Pools every skin's elements into one picker list per slot, so any skin can
/// borrow any other skin's art. Bundled slots pool whole sets; a skin with a
/// partial set contributes only the files it has.
///
/// Tiles are deduplicated by visual content: skins sharing pixel-identical
/// art for a slot collapse into a single tile (the rest are listed in
/// `PoolEntry::also_in`), and fully-transparent placeholders (the blank
/// `cursortrail.png` many skins ship) collapse into one blank tile sorted
/// last — so identical art never repeats down the pool strip.
pub fn build_pool(skins: &[SkinSummary]) -> BTreeMap<&'static str, Vec<PoolEntry>> {
    let mut key_cache = VisualKeyCache::new();
    build_pool_with_keys(skins, &mut key_cache)
}

/// Computes visual keys for every pooled element file in parallel, warming
/// `key_cache` so the sequential pool build below decodes nothing. Files
/// already covered by a stat-valid cache entry are skipped; the rest decode
/// once on a worker thread. Merging the per-worker caches is safe because
/// chunks hold disjoint path sets, and the sequential build re-validates
/// every entry by stat anyway.
fn warm_visual_keys(skins: &[SkinSummary], key_cache: &mut VisualKeyCache) {
    let mut paths: Vec<PathBuf> = skins
        .iter()
        .flat_map(|skin| skin.elements.values().cloned())
        .collect();
    paths.sort_unstable();
    paths.dedup();
    if paths.is_empty() {
        return;
    }
    let thread_count = std::thread::available_parallelism()
        .map(|threads| threads.get())
        .unwrap_or(4)
        .clamp(1, 8);
    let fresh: Vec<VisualKeyCache> = {
        let seeded = &*key_cache;
        std::thread::scope(|scope| {
            let chunk_size = paths.len().div_ceil(thread_count).max(1);
            let handles: Vec<_> = paths
                .chunks(chunk_size)
                .map(|chunk| {
                    scope.spawn(move || {
                        let mut local = VisualKeyCache::new();
                        for path in chunk {
                            let fingerprint = stat_fingerprint(path);
                            if fingerprint.as_ref().is_some_and(|fp| {
                                seeded
                                    .get(path)
                                    .is_some_and(|entry| entry.fingerprint == *fp)
                            }) {
                                continue;
                            }
                            let (key, is_blank) = member_visual_key(path);
                            if let Some(fp) = fingerprint {
                                local.insert(
                                    path.clone(),
                                    VisualKeyEntry {
                                        fingerprint: fp,
                                        key,
                                        is_blank,
                                    },
                                );
                            }
                        }
                        local
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("visual key worker panicked"))
                .collect()
        })
    };
    for local in fresh {
        key_cache.extend(local);
    }
}

/// Pool build that reuses cached visual-content keys (`key_cache`) instead
/// of decoding every image again. Files whose size/mtime changed (or that
/// were never seen) are decoded and their keys stored back into the cache,
/// so a rescan with unchanged skins does no image I/O at all. A scan warms
/// the cache with [`warm_visual_keys`] first, which moves the decode cost
/// onto worker threads; direct callers pay it inline.
fn build_pool_with_keys(
    skins: &[SkinSummary],
    key_cache: &mut VisualKeyCache,
) -> BTreeMap<&'static str, Vec<PoolEntry>> {
    let mut pool: BTreeMap<&'static str, Vec<PoolEntry>> = BTreeMap::new();
    // Slot key → dedup key → index into that slot's pool list.
    let mut key_index: BTreeMap<&'static str, HashMap<String, usize>> = BTreeMap::new();
    for skin in skins {
        for group in active_groups() {
            for slot in group.slots {
                let files: BTreeMap<&'static str, PathBuf> = slot
                    .files
                    .iter()
                    .filter_map(|file| skin.elements.get(file).map(|path| (*file, path.clone())))
                    .collect();
                if files.is_empty() {
                    continue;
                }
                let thumb = files
                    .get(slot.preview_file)
                    .or_else(|| files.values().next())
                    .cloned()
                    .expect("pool entry has at least one file");
                let (dedup_key, is_blank) = entry_dedup_key_cached(&files, key_cache);
                let list = pool.entry(slot.key).or_default();
                let index_for_key = key_index.entry(slot.key).or_default();
                if let Some(&index) = index_for_key.get(&dedup_key) {
                    // Identical art seen before — fold this skin into the tile.
                    let existing = &mut list[index];
                    if existing.skin != skin.folder_name
                        && !existing.also_in.contains(&skin.folder_name)
                    {
                        existing.also_in.push(skin.folder_name.clone());
                    }
                } else {
                    index_for_key.insert(dedup_key, list.len());
                    list.push(PoolEntry {
                        skin: skin.folder_name.clone(),
                        files,
                        thumb,
                        also_in: Vec::new(),
                        is_blank,
                    });
                }
            }
        }
    }
    // Blanks sort last so real art comes first; the rest keeps skin order.
    for entries in pool.values_mut() {
        entries.sort_by_key(|entry| (entry.is_blank, entry.skin.to_lowercase()));
    }
    pool
}

/// Dedup key for one pooled slot entry plus whether it is blank.
/// The key encodes which canonical members are present and each member's
/// visual content, so partial sets never collapse into full ones, but
/// pixel-identical art does — even when the PNG bytes differ through
/// re-encoding. Fully-transparent members all share the `blank` sub-key,
/// so every skin's placeholder collapses into a single blank tile.
fn entry_dedup_key_cached(
    files: &BTreeMap<&'static str, PathBuf>,
    key_cache: &mut VisualKeyCache,
) -> (String, bool) {
    let mut parts = Vec::with_capacity(files.len());
    let mut all_blank = true;
    for (member, path) in files {
        let (sub_key, blank) = visual_key_cached(path, key_cache);
        all_blank &= blank;
        parts.push(format!("{member}={sub_key}"));
    }
    (parts.join("|"), all_blank)
}

/// Image file fingerprint: size plus mtime. Cheap (`metadata` only) and
/// enough to tell whether a cached visual key is still valid.
fn stat_fingerprint(path: &Path) -> Option<(u64, u64, u32)> {
    let meta = fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    Some((meta.len(), mtime.as_secs(), mtime.subsec_nanos()))
}

/// Decoded-content key for one pooled file, valid while its size/mtime
/// match `key_cache`. Decodes (and records) on miss only.
fn visual_key_cached(path: &Path, key_cache: &mut VisualKeyCache) -> (String, bool) {
    let fingerprint = stat_fingerprint(path);
    if let (Some(fingerprint), Some(entry)) = (fingerprint, key_cache.get(path))
        && entry.fingerprint == fingerprint
    {
        return (entry.key.clone(), entry.is_blank);
    }
    let (key, is_blank) = member_visual_key(path);
    if let Some(fingerprint) = fingerprint {
        key_cache.insert(
            path.to_path_buf(),
            VisualKeyEntry {
                fingerprint,
                key: key.clone(),
                is_blank,
            },
        );
    }
    (key, is_blank)
}

/// Visual identity of one image file: `blank` when no opaque pixel survives
/// (alpha ≤ 8 everywhere, same threshold as the preview trim), otherwise the
/// md5 of the trimmed pixels plus their dimensions. Undecodable-but-readable
/// files fall back to their byte md5 (exact duplicates still collapse);
/// unreadable files stay unique so nothing ever wrongly merges.
fn member_visual_key(path: &Path) -> (String, bool) {
    let Ok(bytes) = fs::read(path) else {
        return (format!("unreadable:{}", path.display()), false);
    };
    let Ok(decoded) = image::load_from_memory(&bytes) else {
        return (format!("bytes:{:x}", md5::compute(&bytes)), false);
    };
    let (trimmed, visible) = trim_transparent(&decoded.to_rgba8());
    if !visible {
        return ("blank".to_owned(), true);
    }
    let (width, height) = (trimmed.width(), trimmed.height());
    let digest = md5::compute(trimmed.as_raw());
    (format!("px:{digest:x}:{width}x{height}"), false)
}

// ── Disk cache ───────────────────────────────────────────────────────────────
///
/// The skin scan is slow for one reason: pool dedup decodes every pooled
/// image to key it by visual content. `skin_cache.json` (next to the other
/// app state under `<osu root>/.osu-map-manager`) remembers each skin's file
/// fingerprint plus those content keys, so a startup with unchanged skins
/// only stats files and decodes nothing. Skins whose fingerprint changed
/// (added/removed/modified files) are re-scanned on their own.
const SKIN_CACHE_VERSION: u32 = 1;

/// One cached visual-content key: the file fingerprint it was decoded from
/// plus the key itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredVisualKey {
    size: u64,
    mtime_secs: u64,
    mtime_nanos: u32,
    key: String,
    is_blank: bool,
}

/// (relative path, size, mtime secs, mtime nanos) of every file the skin
/// scan can see, sorted. Equality means the skin is unchanged.
type SkinFingerprint = Vec<(String, u64, u64, u32)>;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedSkin {
    folder_name: String,
    display_name: String,
    author: Option<String>,
    combo_colours: Vec<[u8; 3]>,
    render_opts: SkinRenderOpts,
    file_count: usize,
    /// Canonical element file → skin-relative path (`/` separators).
    elements: BTreeMap<String, String>,
    failsound: Vec<String>,
    failsound_hidden: Vec<String>,
    /// Sorted file list; equality means the skin is unchanged.
    files: SkinFingerprint,
    /// Skin-relative path → content key.
    visual_keys: BTreeMap<String, StoredVisualKey>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SkinCacheFile {
    version: u32,
    skins_dir: PathBuf,
    skins: Vec<CachedSkin>,
}

/// In-memory visual keys: absolute path → entry. Seeded from the disk
/// cache; misses decode and record, hits skip image I/O entirely.
#[derive(Debug, Clone)]
struct VisualKeyEntry {
    fingerprint: (u64, u64, u32),
    key: String,
    is_blank: bool,
}

type VisualKeyCache = HashMap<PathBuf, VisualKeyEntry>;

/// Canonical slot file (`hitcircle.png`, `default-2.png`, …) by name, so
/// cached element maps resolve back to their `&'static str` keys.
fn slot_file_by_name(name: &str) -> Option<&'static str> {
    active_groups()
        .flat_map(|group| group.slots)
        .flat_map(|slot| slot.files.iter())
        .copied()
        .find(|file| *file == name)
}

/// Skin-relative path with forward slashes (case preserved, so it rebases
/// on case-sensitive filesystems too).
fn skin_relpath_posix(path: &Path, skin_root: &Path) -> Option<String> {
    path.strip_prefix(skin_root)
        .ok()
        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
}

fn file_fingerprint(skin_root: &Path, path: &Path) -> Option<(String, u64, u64, u32)> {
    let (size, secs, nanos) = stat_fingerprint(path)?;
    let rel = skin_relpath_posix(path, skin_root)?;
    Some((rel, size, secs, nanos))
}

/// Every file that can affect the scan outcome, and nothing else: all root
/// files (skin.ini → metadata, failsound names, slot matches, file count)
/// plus nested files whose names resolve to a slot (same matching rule as
/// [`file_matches_slot`, same depth/dot-folder walk as
/// [`collect_nested_candidates`]). Storyboards, hitsounds and other nested
/// junk never influence scan results, so skipping them keeps verification
/// to a directory walk plus a handful of stats per skin instead of one
/// metadata call per file.
fn fingerprint_skin(skin_root: &Path, slot_stems: &HashSet<&str>) -> SkinFingerprint {
    let mut out = Vec::new();
    if let Ok(read_dir) = fs::read_dir(skin_root) {
        for entry in read_dir.filter_map(|entry| entry.ok()) {
            let path = entry.path();
            if path.is_file()
                && let Some(fp) = file_fingerprint(skin_root, &path)
            {
                out.push(fp);
            }
        }
    }
    for (name, path, _) in collect_nested_candidates(skin_root) {
        if !nested_name_matters(&name, slot_stems) {
            continue;
        }
        if let Some(fp) = file_fingerprint(skin_root, &path) {
            out.push(fp);
        }
    }
    out.sort();
    out
}

/// Mirrors [`file_matches_slot`]: could this nested file name ever resolve
/// to a slot element (plain, `@2x`, or a prefixed digit, whose stem always
/// matches its slot)? Files that can't match never affect the scan.
fn nested_name_matters(file_name: &str, slot_stems: &HashSet<&str>) -> bool {
    let lower = file_name.to_ascii_lowercase();
    let (stem, ext) = match lower.rsplit_once('.') {
        Some((stem, ext)) => (stem, ext),
        None => return false,
    };
    let stem = stem.strip_suffix("@2x").unwrap_or(stem);
    matches!(ext, "png" | "jpg" | "jpeg") && slot_stems.contains(stem)
}

fn summary_from_cache(skins_dir: &Path, cached: &CachedSkin) -> SkinSummary {
    let root = skins_dir.join(&cached.folder_name);
    let join = |rel: &String| root.join(rel);
    SkinSummary {
        display_name: cached.display_name.clone(),
        folder_name: cached.folder_name.clone(),
        path: root.clone(),
        author: cached.author.clone(),
        combo_colours: cached
            .combo_colours
            .iter()
            .map(|[r, g, b]| egui::Color32::from_rgb(*r, *g, *b))
            .collect(),
        render_opts: cached.render_opts,
        file_count: cached.file_count,
        elements: cached
            .elements
            .iter()
            .filter_map(|(canonical, rel)| {
                slot_file_by_name(canonical).map(|file| (file, join(rel)))
            })
            .collect(),
        failsound: cached.failsound.iter().map(join).collect(),
        failsound_hidden: cached.failsound_hidden.iter().map(join).collect(),
    }
}

fn cached_skin_from_summary(
    summary: &SkinSummary,
    fingerprint: SkinFingerprint,
    key_cache: &VisualKeyCache,
) -> CachedSkin {
    let rel = |path: &Path| {
        skin_relpath_posix(path, &summary.path).unwrap_or_else(|| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
    };
    let colours = summary
        .combo_colours
        .iter()
        .map(|colour| {
            let [r, g, b, _] = colour.to_array();
            [r, g, b]
        })
        .collect();
    let mut visual_keys = BTreeMap::new();
    for (path, entry) in key_cache {
        if let Some(rel) = skin_relpath_posix(path, &summary.path) {
            visual_keys.insert(
                rel,
                StoredVisualKey {
                    size: entry.fingerprint.0,
                    mtime_secs: entry.fingerprint.1,
                    mtime_nanos: entry.fingerprint.2,
                    key: entry.key.clone(),
                    is_blank: entry.is_blank,
                },
            );
        }
    }
    CachedSkin {
        folder_name: summary.folder_name.clone(),
        display_name: summary.display_name.clone(),
        author: summary.author.clone(),
        combo_colours: colours,
        render_opts: summary.render_opts,
        file_count: summary.file_count,
        elements: summary
            .elements
            .iter()
            .map(|(canonical, path)| (canonical.to_string(), rel(path)))
            .collect(),
        failsound: summary.failsound.iter().map(|path| rel(path)).collect(),
        failsound_hidden: summary
            .failsound_hidden
            .iter()
            .map(|path| rel(path))
            .collect(),
        files: fingerprint,
        visual_keys,
    }
}

fn load_skin_cache(cache_path: &Path, skins_dir: &Path) -> Option<SkinCacheFile> {
    let bytes = fs::read(cache_path).ok()?;
    let cache: SkinCacheFile = serde_json::from_slice(&bytes).ok()?;
    if cache.version != SKIN_CACHE_VERSION || cache.skins_dir != skins_dir {
        return None;
    }
    Some(cache)
}

fn save_skin_cache(cache_path: &Path, cache: &SkinCacheFile) -> Result<()> {
    if let Some(parent) = cache_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let bytes = serde_json::to_vec(cache).with_context(|| "serializing the skin cache")?;
    fs::write(cache_path, bytes).with_context(|| format!("writing {}", cache_path.display()))?;
    Ok(())
}

/// One skin folder processed by a scan worker: either rebuilt from the
/// disk cache or freshly scanned, plus the fingerprint that decided it.
struct SkinWorkItem {
    folder_name: String,
    fingerprint: SkinFingerprint,
    summary: SkinSummary,
    /// The cache entry this was rebuilt from, if any — re-saved as-is.
    cached: Option<CachedSkin>,
}

/// Fingerprint one skin and rebuild it from `cached` when unchanged,
/// otherwise scan it fresh. Skins are independent, so scan workers run
/// this in parallel.
fn process_one_skin(
    skins_dir: &Path,
    cached: &HashMap<String, CachedSkin>,
    slot_stems: &HashSet<&str>,
    folder_name: String,
    root: PathBuf,
) -> SkinWorkItem {
    let fingerprint = fingerprint_skin(&root, slot_stems);
    match cached.get(&folder_name) {
        Some(cached_skin) if cached_skin.files == fingerprint => SkinWorkItem {
            folder_name,
            fingerprint,
            summary: summary_from_cache(skins_dir, cached_skin),
            cached: Some(cached_skin.clone()),
        },
        _ => {
            let summary = scan_single_skin(&root, folder_name.clone());
            SkinWorkItem {
                folder_name,
                fingerprint,
                summary,
                cached: None,
            }
        }
    }
}

/// Full skin scan with a disk cache: unchanged skins are rebuilt from the
/// cache (no image decodes), changed ones are re-scanned, and the pool is
/// built with cached visual keys. Per-skin work runs on a small thread
/// pool; the whole scan itself already runs on a background thread.
fn run_cached_skin_scan(skins_dir: PathBuf, cache_path: Option<PathBuf>) -> SkinScanResult {
    // No cache location (should not normally happen when a Skins folder is
    // set): plain uncached scan, same as before the disk cache existed.
    if cache_path.is_none() {
        let skins = scan_skins(&skins_dir);
        let pool = build_pool(&skins);
        return SkinScanResult {
            dir: Some(skins_dir),
            skins,
            pool,
        };
    }
    let cached: HashMap<String, CachedSkin> = cache_path
        .as_deref()
        .and_then(|path| load_skin_cache(path, &skins_dir))
        .map(|cache| {
            cache
                .skins
                .into_iter()
                .map(|skin| (skin.folder_name.clone(), skin))
                .collect()
        })
        .unwrap_or_default();

    // Seed visual keys from the cache; per-file fingerprints are
    // re-validated on use, so stale entries can never wrongly match.
    let mut key_cache = VisualKeyCache::new();
    for cached_skin in cached.values() {
        let root = skins_dir.join(&cached_skin.folder_name);
        for (rel, stored) in &cached_skin.visual_keys {
            key_cache.insert(
                root.join(rel),
                VisualKeyEntry {
                    fingerprint: (stored.size, stored.mtime_secs, stored.mtime_nanos),
                    key: stored.key.clone(),
                    is_blank: stored.is_blank,
                },
            );
        }
    }

    // Canonical slot stems, so fingerprinting can tell at a glance which
    // nested files are able to resolve to an element.
    let slot_stems: HashSet<&str> = active_groups()
        .flat_map(|group| group.slots)
        .flat_map(|slot| slot.files.iter())
        .map(|file| slot_stem(file))
        .collect();

    let mut folders: Vec<(String, PathBuf)> = Vec::new();
    if let Ok(entries) = fs::read_dir(&skins_dir) {
        for entry in entries.filter_map(|entry| entry.ok()) {
            let root = entry.path();
            if !root.is_dir() {
                continue;
            }
            let folder_name = entry.file_name().to_string_lossy().to_string();
            if folder_name.starts_with('.') {
                continue;
            }
            folders.push((folder_name, root));
        }
    }

    // Fingerprinting is I/O-bound and fresh skins decode images: spread
    // folders across workers instead of walking them one by one.
    let thread_count = std::thread::available_parallelism()
        .map(|threads| threads.get())
        .unwrap_or(4)
        .clamp(1, 8);
    let mut items: Vec<SkinWorkItem> = Vec::with_capacity(folders.len());
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        let chunk_size = folders.len().div_ceil(thread_count).max(1);
        for chunk in folders.chunks(chunk_size) {
            handles.push(scope.spawn(|| {
                chunk
                    .iter()
                    .map(|(folder, root)| {
                        process_one_skin(
                            &skins_dir,
                            &cached,
                            &slot_stems,
                            folder.clone(),
                            root.clone(),
                        )
                    })
                    .collect::<Vec<_>>()
            }));
        }
        for handle in handles {
            items.extend(handle.join().expect("skin scan worker panicked"));
        }
    });
    items.sort_by_key(|item| item.folder_name.to_lowercase());

    let mut skins = Vec::with_capacity(items.len());
    // Folder names whose skins were rebuilt from the cache and can be
    // re-saved as-is; everything else is freshly scanned below.
    let mut reusable: HashMap<String, CachedSkin> = HashMap::new();
    let mut fresh_fps: HashMap<String, SkinFingerprint> = HashMap::new();
    for item in items {
        let SkinWorkItem {
            folder_name,
            fingerprint,
            summary,
            cached,
        } = item;
        match cached {
            Some(cached_skin) => {
                reusable.insert(folder_name, cached_skin);
            }
            None => {
                fresh_fps.insert(folder_name, fingerprint);
            }
        }
        skins.push(summary);
    }

    warm_visual_keys(&skins, &mut key_cache);
    let pool = build_pool_with_keys(&skins, &mut key_cache);

    // Persist only when something actually changed: a fully cached scan
    // would just rewrite the identical cache file.
    if !fresh_fps.is_empty()
        && let Some(cache_path) = cache_path
    {
        // Reused entries as-is, fresh ones from the new summaries plus
        // the keys the pool build just decoded.
        let mut out = Vec::with_capacity(skins.len());
        for skin in &skins {
            if let Some(cached_skin) = reusable.remove(&skin.folder_name) {
                out.push(cached_skin);
            } else if let Some(fingerprint) = fresh_fps.remove(&skin.folder_name) {
                out.push(cached_skin_from_summary(skin, fingerprint, &key_cache));
            }
        }
        let cache = SkinCacheFile {
            version: SKIN_CACHE_VERSION,
            skins_dir: skins_dir.clone(),
            skins: out,
        };
        let _ = save_skin_cache(&cache_path, &cache);
    }
    SkinScanResult {
        dir: Some(skins_dir),
        skins,
        pool,
    }
}

// ── Failsound ────────────────────────────────────────────────────────────────

/// Audio containers the game loads a skin `failsound` from.
const FAILSOUND_EXTS: &[&str] = &["mp3", "ogg", "wav"];

/// Splits a skin's root listing into live vs. hidden `failsound` audio files.
///
/// Live files (`failsound.mp3`/`.ogg`/`.wav`, any case) are what the game
/// plays on fail; hidden ones carry one extra `.bak` suffix
/// (`failsound.mp3.bak`) — the game ignores them, so hiding is a reversible
/// mute. Matching is limited to the skin root, since that is the only place
/// the game loads skin sounds from — nested files, the `sectionfail.png`
/// banner and non-audio namesakes are left alone.
fn failsound_from_root(files: &[(String, PathBuf)]) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut live = Vec::new();
    let mut hidden = Vec::new();
    for (name, path) in files {
        let lower = name.to_ascii_lowercase();
        let (base, is_hidden) = match lower.rsplit_once('.') {
            Some((stem, "bak")) => (stem, true),
            _ => (lower.as_str(), false),
        };
        let is_audio = match base.rsplit_once('.') {
            Some((stem, ext)) => stem == "failsound" && FAILSOUND_EXTS.contains(&ext),
            None => false,
        };
        if is_audio {
            if is_hidden {
                hidden.push(path.clone());
            } else {
                live.push(path.clone());
            }
        }
    }
    live.sort();
    hidden.sort();
    (live, hidden)
}

/// Hides `failsound` files by appending `.bak` (`failsound.mp3` →
/// `failsound.mp3.bak`), so the game stops picking them up while they stay
/// restorable in place. A stale backup of the same file is replaced.
/// Returns how many were hidden; bails on the first IO error so a half-done
/// run is reported instead of silently kept.
fn hide_failsound_files(paths: &[PathBuf]) -> Result<usize> {
    let mut count = 0;
    for path in paths {
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .with_context(|| format!("non-unicode file name {}", path.display()))?;
        let backup = path.with_file_name(format!("{file_name}.bak"));
        if backup.exists() {
            fs::remove_file(&backup)
                .with_context(|| format!("replacing stale backup {}", backup.display()))?;
        }
        fs::rename(path, &backup)
            .with_context(|| format!("hiding {} as {}", path.display(), backup.display()))?;
        count += 1;
    }
    Ok(count)
}

/// Restores hidden `failsound` files (`failsound.mp3.bak` →
/// `failsound.mp3`). Refuses to overwrite a live file of the same name —
/// hide or remove that one first. Returns how many were restored.
fn restore_failsound_files(paths: &[PathBuf]) -> Result<usize> {
    let mut count = 0;
    for backup in paths {
        let file_name = backup
            .file_name()
            .and_then(|name| name.to_str())
            .with_context(|| format!("non-unicode file name {}", backup.display()))?;
        // ".bak" is ASCII, so byte slicing at the suffix boundary is safe;
        // the comparison itself stays case-insensitive (`*.BAK` included).
        let live_name = if file_name.len() > 4
            && file_name[file_name.len() - 4..].eq_ignore_ascii_case(".bak")
        {
            &file_name[..file_name.len() - 4]
        } else {
            anyhow::bail!("{} is not a hidden backup", backup.display());
        };
        let live = backup.with_file_name(live_name);
        if live.exists() {
            anyhow::bail!(
                "{} already exists — hide or remove it before restoring {}",
                live.display(),
                backup.display()
            );
        }
        fs::rename(backup, &live).with_context(|| format!("restoring {}", backup.display()))?;
        count += 1;
    }
    Ok(count)
}

// ── Versioning & saving ──────────────────────────────────────────────────────

/// `"Cool Skin v3"` → `("Cool Skin", Some(3))`; no suffix → `(name, None)`.
fn split_version_suffix(name: &str) -> (&str, Option<u32>) {
    if let Some(position) = name.rfind(" v")
        && let Ok(version) = name[position + 2..].parse::<u32>()
    {
        return (&name[..position], Some(version));
    }
    (name, None)
}

/// Next free `"<base> v<N>"` name: starts at v1, or one past the base's own
/// version suffix, skipping versions that already exist on disk.
pub fn next_versioned_name(base: &str, skins_dir: &Path) -> String {
    let (clean, version) = split_version_suffix(base.trim_end());
    let clean = clean.trim_end();
    let mut version = version.map_or(1, |v| v.saturating_add(1));
    while !clean.is_empty() && skins_dir.join(format!("{clean} v{version}")).exists() {
        version += 1;
    }
    format!("{clean} v{version}")
}

/// Rewrites `Name:`/`Author:` inside `[General]`, keeping everything else
/// verbatim; a missing file or section gets a minimal header prepended.
fn patch_skin_ini(original: Option<&str>, name: &str, author: Option<&str>) -> String {
    let author = author.unwrap_or("");
    let mut lines: Vec<String> = original.unwrap_or("").lines().map(str::to_owned).collect();
    let Some(general_pos) = lines
        .iter()
        .position(|line| line.trim().eq_ignore_ascii_case("[general]"))
    else {
        return format!(
            "[General]\nName: {name}\nAuthor: {author}\n\n{}",
            original.unwrap_or("")
        );
    };

    let section_end = lines[general_pos + 1..]
        .iter()
        .position(|line| {
            let trimmed = line.trim();
            trimmed.starts_with('[') && trimmed.ends_with(']')
        })
        .map_or(lines.len(), |offset| general_pos + 1 + offset);

    let mut replaced_name = false;
    let mut replaced_author = false;
    for line in &mut lines[general_pos + 1..section_end] {
        if let Some((key, _)) = line.split_once(':') {
            if key.trim().eq_ignore_ascii_case("name") {
                *line = format!("Name: {name}");
                replaced_name = true;
            } else if key.trim().eq_ignore_ascii_case("author") {
                *line = format!("Author: {author}");
                replaced_author = true;
            }
        }
    }
    let mut missing = Vec::new();
    if !replaced_name {
        missing.push(format!("Name: {name}"));
    }
    if !replaced_author {
        missing.push(format!("Author: {author}"));
    }
    for (offset, line) in missing.iter().enumerate() {
        lines.insert(section_end + offset, line.clone());
    }

    let mut out = lines.join("\n");
    out.push('\n');
    out
}

#[derive(Debug)]
pub struct SaveOutcome {
    pub path: PathBuf,
    pub name: String,
    pub files_copied: usize,
    pub overrides_applied: usize,
    /// Member files written with their artwork scaled inside the original
    /// canvas resolution (`resizes`).
    pub resizes_applied: usize,
}

/// Writes `<base> v<N>` next to the base skin: a full copy of the base folder
/// (so the skin loads in game), a skin.ini whose `Name:` is the new version
/// name, then every picked pool asset copied in under its canonical element
/// name (existing variants of that element are removed first, since the game
/// would prefer a leftover `@2x`). Members a partial set pick doesn't include
/// keep the base skin's files, matching the game's fallback behaviour.
///
/// `resizes` (slot key → scale) scales the artwork of the slot's files inside
/// their original canvas resolution — the file keeps its exact pixel size, so
/// the game still draws it at the same place, only the art within it shrinks
/// or grows. Slots with a pooled pick scale the picked file; the rest scale
/// the base skin's own file. The artwork is anchored to the canvas centre
/// (`CursorCentre` skins) or its top-left (the game puts the canvas corner on
/// the pointer there), following the base skin's `CursorCentre` setting.
pub fn save_skin(
    base: &SkinSummary,
    overrides: &[(String, PoolEntry)],
    resizes: &[(String, f32)],
) -> Result<SaveOutcome> {
    let skins_dir = base
        .path
        .parent()
        .with_context(|| format!("skin folder {} has no parent", base.path.display()))?
        .to_path_buf();
    let target_name = next_versioned_name(&base.folder_name, &skins_dir);
    let target = skins_dir.join(&target_name);
    fs::create_dir_all(&target)
        .with_context(|| format!("creating skin folder {}", target.display()))?;

    let mut files_copied = 0;
    for relative in collect_files_recursive(&base.path)? {
        let source = base.path.join(&relative);
        let destination = target.join(&relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        fs::copy(&source, &destination).with_context(|| format!("copying {}", source.display()))?;
        files_copied += 1;
    }

    let ini_path = target.join("skin.ini");
    let original = read_skin_text(&ini_path);
    fs::write(
        &ini_path,
        patch_skin_ini(original.as_deref(), &target_name, base.author.as_deref()),
    )
    .with_context(|| format!("writing {}", ini_path.display()))?;

    let mut overrides_applied = 0;
    let mut resizes_applied = 0;
    // Only real changes count: 100% entries never reach the map, but guard
    // against float noise anyway.
    let resize_by_slot: HashMap<&str, f32> = resizes
        .iter()
        .filter(|(_, scale)| (scale - 1.0).abs() > 1e-3)
        .map(|(slot_key, scale)| (slot_key.as_str(), *scale))
        .collect();
    let centre_anchor = base.render_opts.cursor_centre;

    for (slot_key, entry) in overrides {
        let scale = resize_by_slot.get(slot_key.as_str()).copied();
        for (member_file, path) in &entry.files {
            remove_slot_variants(&target, slot_stem(member_file));
            let source_name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("element.png");
            let destination = target.join(override_dest_name(member_file, source_name));
            match scale {
                Some(scale) => {
                    write_scaled_image(path, &destination, scale, centre_anchor)
                        .with_context(|| format!("resizing {}", path.display()))?;
                    resizes_applied += 1;
                }
                None => {
                    fs::copy(path, &destination)
                        .with_context(|| format!("copying pooled asset {}", path.display()))?;
                }
            }
            overrides_applied += 1;
        }
    }

    // Resized slots without a pooled pick scale the base skin's own file,
    // overwriting the plain folder copy at the same relative path.
    for (slot_key, scale) in &resize_by_slot {
        if overrides.iter().any(|(slot, _)| slot == slot_key) {
            continue;
        }
        let Some(slot) = find_slot(slot_key) else {
            continue;
        };
        for member_file in slot.files {
            let Some(source) = base.elements.get(member_file) else {
                continue;
            };
            let Ok(relative) = source.strip_prefix(&base.path) else {
                continue;
            };
            let destination = target.join(relative);
            remove_slot_variants(&target, slot_stem(member_file));
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            write_scaled_image(source, &destination, *scale, centre_anchor)
                .with_context(|| format!("resizing {}", source.display()))?;
            resizes_applied += 1;
        }
    }

    Ok(SaveOutcome {
        path: target,
        name: target_name,
        files_copied,
        overrides_applied,
        resizes_applied,
    })
}

/// The editor slot table entry for a slot key.
fn find_slot(slot_key: &str) -> Option<&'static Slot> {
    active_groups()
        .flat_map(|group| group.slots)
        .find(|slot| slot.key == slot_key)
}

/// Scales an image's artwork by `scale` inside its original canvas: the whole
/// image is resized and pasted back onto a same-size transparent canvas, so
/// the file's resolution never changes. `centre_anchor` centres the scaled
/// art (`CursorCentre` skins keep the art on the pointer); otherwise it stays
/// pinned to the top-left corner. Upscaling clips at the canvas edges.
fn scale_within_canvas(
    image: &image::RgbaImage,
    scale: f32,
    centre_anchor: bool,
) -> image::RgbaImage {
    let (width, height) = image.dimensions();
    let scaled_w = ((width as f32 * scale).round() as u32).max(1);
    let scaled_h = ((height as f32 * scale).round() as u32).max(1);
    let resized = image::imageops::resize(
        image,
        scaled_w,
        scaled_h,
        image::imageops::FilterType::CatmullRom,
    );
    let mut canvas = image::RgbaImage::new(width, height);
    // `overlay` clips at the canvas bounds, so upscaling crops to the anchor.
    let offset_x = if centre_anchor {
        (i64::from(width) - i64::from(scaled_w)) / 2
    } else {
        0
    };
    let offset_y = if centre_anchor {
        (i64::from(height) - i64::from(scaled_h)) / 2
    } else {
        0
    };
    image::imageops::overlay(&mut canvas, &resized, offset_x, offset_y);
    canvas
}

/// Decodes `source`, scales its artwork inside the original canvas (see
/// `scale_within_canvas`) and writes it to `dest` in the file's own format.
fn write_scaled_image(source: &Path, dest: &Path, scale: f32, centre_anchor: bool) -> Result<()> {
    let decoded = image::io::Reader::open(source)
        .with_context(|| format!("opening {}", source.display()))?
        .with_guessed_format()
        .with_context(|| format!("detecting format of {}", source.display()))?
        .decode()
        .with_context(|| format!("decoding {}", source.display()))?
        .to_rgba8();
    let scaled = scale_within_canvas(&decoded, scale, centre_anchor);
    let extension = dest
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase());
    match extension.as_deref() {
        // JPEG has no alpha — flatten onto black, the game treats jpg
        // elements as opaque anyway.
        Some("jpg") | Some("jpeg") => image::DynamicImage::ImageRgba8(scaled)
            .to_rgb8()
            .save_with_format(dest, image::ImageFormat::Jpeg)
            .with_context(|| format!("writing {}", dest.display()))?,
        _ => scaled
            .save_with_format(dest, image::ImageFormat::Png)
            .with_context(|| format!("writing {}", dest.display()))?,
    }
    Ok(())
}

/// The pooled file keeps its own container (`default-2@2x.png` →
/// `default-2.png`): the canonical stem with the source extension.
fn override_dest_name(slot_file: &str, source_file_name: &str) -> String {
    let lower = source_file_name.to_ascii_lowercase();
    let ext = lower.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("png");
    format!("{}.{ext}", slot_stem(slot_file))
}

/// Deletes `<stem>.png` / `<stem>@2x.jpg`-style leftovers so the game loads
/// the freshly copied element instead of a higher-res variant of the old one.
fn remove_slot_variants(target: &Path, stem: &str) {
    let Ok(read_dir) = fs::read_dir(target) else {
        return;
    };
    for entry in read_dir.filter_map(|entry| entry.ok()) {
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        let Some((file_stem, ext)) = name.rsplit_once('.') else {
            continue;
        };
        let base_stem = file_stem.strip_suffix("@2x").unwrap_or(file_stem);
        if base_stem == stem && matches!(ext, "png" | "jpg" | "jpeg") {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn collect_files_recursive(root: &Path) -> Result<Vec<PathBuf>> {
    fn walk(dir: &Path, relative: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
            let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
            let path = entry.path();
            let child_relative = relative.join(entry.file_name());
            if path.is_dir() {
                walk(&path, &child_relative, out)?;
            } else {
                out.push(child_relative);
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    walk(root, Path::new(""), &mut files)?;
    files.sort();
    Ok(files)
}

// ── Importing own assets ─────────────────────────────────────────────────────

/// The skin name shown on imported pool tiles.
const IMPORTED_SKIN: &str = "Imported";

/// Image containers the import accepts — the same ones the game loads skin
/// elements from.
const IMPORT_IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg"];

/// Is this file something the import can read, by extension?
fn is_importable_image(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| IMPORT_IMAGE_EXTS.contains(&ext.to_ascii_lowercase().as_str()))
}

/// The slot with this key, so an imported pool entry can pick its thumbnail
/// member.
fn slot_by_key(key: &str) -> Option<&'static Slot> {
    active_groups()
        .flat_map(|group| group.slots)
        .find(|slot| slot.key == key)
}

/// Ranking mirroring the scan's preference among variants of one element:
/// `@2x` over plain, png over the lossy containers among equals.
fn import_variant_rank(file_name: &str) -> (bool, bool) {
    let lower = file_name.to_ascii_lowercase();
    let (stem, ext) = lower.rsplit_once('.').unwrap_or((&lower, ""));
    (stem.ends_with("@2x"), ext == "png")
}

/// Classifies files by file name into element slots: `cursor@2x.png` lands in
/// the `cursor` slot under its canonical name, `default-3.png` in
/// `circle-numbers`, and so on — the same stem/`@2x` matching the skin scan
/// uses. When several variants of one element are imported together
/// (`cursor.png` + `cursor@2x.png`) the game-preferred one wins. Returns the
/// classified files per slot key plus the file names nothing matched (not an
/// importable image, or no active element ships that name — the paused-HUD
/// groups included).
fn classify_imports(
    paths: &[PathBuf],
) -> (
    BTreeMap<&'static str, BTreeMap<&'static str, PathBuf>>,
    Vec<String>,
) {
    let mut placed: BTreeMap<&'static str, BTreeMap<&'static str, PathBuf>> = BTreeMap::new();
    let mut skipped: Vec<String> = Vec::new();
    for path in paths {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let mut matched: Option<(&'static str, &'static str)> = None;
        'outer: for group in active_groups() {
            for slot in group.slots {
                for slot_file in slot.files {
                    if file_matches_slot(name, slot_file) {
                        matched = Some((slot.key, slot_file));
                        break 'outer;
                    }
                }
            }
        }
        let Some((slot_key, slot_file)) = matched else {
            skipped.push(name.to_owned());
            continue;
        };
        let slot_files = placed.entry(slot_key).or_default();
        let rank = import_variant_rank(name);
        let better = match slot_files.get(slot_file) {
            None => true,
            Some(existing) => {
                let existing_rank = existing
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(import_variant_rank)
                    .unwrap_or((false, false));
                rank > existing_rank
            }
        };
        if better {
            slot_files.insert(slot_file, path.clone());
        }
    }
    (placed, skipped)
}

/// Formats the skipped tail of an import status message: how many files
/// nothing matched, with up to five example names.
fn skipped_note(skipped: &[String]) -> String {
    if skipped.is_empty() {
        return String::new();
    }
    const MAX_LISTED: usize = 5;
    let mut note = format!(
        " Skipped {} file(s) with no matching element: {}",
        skipped.len(),
        skipped
            .iter()
            .take(MAX_LISTED)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    );
    if skipped.len() > MAX_LISTED {
        note.push_str(", …");
    }
    note
}

// ── Texture loading ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TexKey {
    path: PathBuf,
    max_dim: u32,
}

const THUMB_DIM: u32 = 128;
const PREVIEW_DIM: u32 = 768;
const MAX_TEXTURES_IN_FLIGHT: usize = 4;
const TEXTURE_CACHE_LIMIT: usize = 900;
/// Resolution of the "Cursor size" slider: 5 % steps, with a detent at the
/// 100 % default (`RESIZE_STEP * 1.5` catch radius on pointer drags).
const RESIZE_STEP: f32 = 0.05;

/// A decoded element: the GPU-ready image plus the metadata the mock needs to
/// size it the way the game would.
#[derive(Debug)]
struct DecodedTexture {
    image: egui::ColorImage,
    /// Any opaque pixel at all? Instafade skins ship circle layers that are
    /// fully transparent — the game draws nothing for them. Only asserted from
    /// tests, hence the allowance.
    #[allow(dead_code)]
    visible: bool,
    /// Trimmed artwork height in 1x game units: file pixels, halved for `@2x`
    /// files (the game draws those at half their pixel size). Computed before
    /// any thumbnail downscale, so it is stable across `max_dim` values.
    game_height: f32,
}

/// `hitcircle@2x.png` → true.
fn path_is_2x(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.to_ascii_lowercase()
                .rsplit_once('.')
                .is_some_and(|(stem, _)| stem.ends_with("@2x"))
        })
}

fn decode_texture(path: &Path, max_dim: u32) -> Result<DecodedTexture> {
    let image = image::io::Reader::open(path)
        .with_context(|| format!("opening {}", path.display()))?
        .with_guessed_format()
        .with_context(|| format!("detecting format of {}", path.display()))?
        .decode()
        .with_context(|| format!("decoding {}", path.display()))?;
    let (rgba, visible) = trim_transparent(&image.to_rgba8());
    let game_height = rgba.height() as f32 / if path_is_2x(path) { 2.0 } else { 1.0 };
    let rgba = if rgba.width().max(rgba.height()) > max_dim {
        image::DynamicImage::ImageRgba8(rgba)
            .thumbnail(max_dim, max_dim)
            .to_rgba8()
    } else {
        rgba
    };
    Ok(DecodedTexture {
        image: egui::ColorImage::from_rgba_unmultiplied(
            [rgba.width() as usize, rgba.height() as usize],
            rgba.as_raw(),
        ),
        visible,
        game_height,
    })
}

/// Skins often ship elements with transparent padding — especially `@2x`-only
/// files whose artwork sits at 1x scale inside a twice-as-large canvas, which
/// would otherwise render at half size next to other skins. Cropping to the
/// opaque bounds keeps every skin's art at the same visual size in previews.
/// Returns the cropped image and whether any opaque pixel was found (a fully
/// transparent layer stays uncropped and reports `false`).
fn trim_transparent(image: &image::RgbaImage) -> (image::RgbaImage, bool) {
    let (width, height) = image.dimensions();
    let mut min_x = width;
    let mut min_y = height;
    let mut max_x = 0_u32;
    let mut max_y = 0_u32;
    let mut opaque = false;
    for (x, y, pixel) in image.enumerate_pixels() {
        if pixel.0[3] > 8 {
            opaque = true;
            min_x = min_x.min(x);
            max_x = max_x.max(x);
            min_y = min_y.min(y);
            max_y = max_y.max(y);
        }
    }
    if !opaque {
        return (image.clone(), false);
    }
    let cropped =
        image::imageops::crop_imm(image, min_x, min_y, max_x - min_x + 1, max_y - min_y + 1)
            .to_image();
    (cropped, true)
}

// ── State ────────────────────────────────────────────────────────────────────

/// A cached element texture plus the decode metadata the scene needs.
#[derive(Clone)]
struct LoadedTexture {
    handle: egui::TextureHandle,
    /// Trimmed artwork height in 1x game units (`DecodedTexture::game_height`).
    game_height: f32,
}

/// Finished background skin scan shipped back to the UI thread.
struct SkinScanResult {
    dir: Option<PathBuf>,
    skins: Vec<SkinSummary>,
    pool: BTreeMap<&'static str, Vec<PoolEntry>>,
}

pub struct SkinEditorState {
    scanned_dir: Option<PathBuf>,
    skins: Vec<SkinSummary>,
    pool: BTreeMap<&'static str, Vec<PoolEntry>>,
    /// Background scan (`scan_skins` + `build_pool`, which decodes image
    /// content for dedup) in flight. While set, the tab shows a spinner
    /// instead of blocking the UI thread on the filesystem.
    scan_in_flight: bool,
    scan_rx: Option<Receiver<SkinScanResult>>,
    selected: Option<usize>,
    /// slot key → pool entry picked by the user (wins over the base skin).
    overrides: BTreeMap<&'static str, PoolEntry>,
    /// Artwork scale for the whole cursor (`CURSOR_SLOT_KEYS` assets are
    /// resized together): the art is scaled inside each file's original
    /// canvas on save. 1.0 = unchanged.
    cursor_resize: f32,
    force_rescan: bool,
    tex: HashMap<TexKey, LoadedTexture>,
    tex_order: Vec<TexKey>,
    failed: HashSet<TexKey>,
    in_flight: HashSet<TexKey>,
    load_tx: Sender<(TexKey, Result<DecodedTexture, String>)>,
    load_rx: Receiver<(TexKey, Result<DecodedTexture, String>)>,
    is_saving: bool,
    save_rx: Option<Receiver<Result<SaveOutcome>>>,
    save_status: Option<String>,
    save_ok: bool,
    failsound_status: Option<String>,
    failsound_ok: bool,
    /// Skin path the status message above belongs to. Without the pin,
    /// hiding A's failsound then selecting B would still show A's message
    /// under B.
    failsound_status_for: Option<PathBuf>,
    /// Skin path the armed failsound hide belongs to. Hiding needs two
    /// clicks (arm, then confirm); pinning the arm to the skin path disarms
    /// it as soon as another skin is selected.
    failsound_armed_for: Option<PathBuf>,
    /// Skin folder to select once the in-flight scan lands (used after
    /// saving, which creates a new versioned folder the old index can't
    /// point at).
    pending_select_folder: Option<String>,
    /// Slot key → imported pool entries ("Imported" tiles), merged into the
    /// slot rows' pool strips and kept across rescans.
    imported: BTreeMap<&'static str, Vec<PoolEntry>>,
    /// Last import result message (`import_ok` picks its colour).
    import_status: Option<String>,
    import_ok: bool,
}

impl SkinEditorState {
    pub fn new() -> Self {
        let (load_tx, load_rx) = mpsc::channel();
        Self {
            scanned_dir: None,
            skins: Vec::new(),
            pool: BTreeMap::new(),
            scan_in_flight: false,
            scan_rx: None,
            selected: None,
            overrides: BTreeMap::new(),
            cursor_resize: 1.0,
            force_rescan: false,
            tex: HashMap::new(),
            tex_order: Vec::new(),
            failed: HashSet::new(),
            in_flight: HashSet::new(),
            load_tx,
            load_rx,
            is_saving: false,
            save_rx: None,
            save_status: None,
            save_ok: false,
            failsound_status: None,
            failsound_ok: false,
            failsound_status_for: None,
            failsound_armed_for: None,
            pending_select_folder: None,
            imported: BTreeMap::new(),
            import_status: None,
            import_ok: false,
        }
    }

    /// Rescans when the Songs folder (and with it the Skins folder) changed
    /// or a rescan was requested; cheap no-op otherwise.
    ///
    /// The scan (filesystem walk plus image decodes for pool dedup) runs on
    /// a background thread so switching to this tab never freezes the UI.
    /// Unchanged skins are rebuilt from the disk cache (`cache_path`), so
    /// only new or modified skins pay for decodes. While a scan is in
    /// flight the tab shows a spinner until the results land.
    pub fn ensure_scanned(&mut self, skins_dir: Option<&Path>, cache_path: Option<&Path>) {
        self.poll_scan_result();
        if self.scan_in_flight {
            return;
        }
        if !self.force_rescan && self.scanned_dir.as_deref() == skins_dir {
            return;
        }
        self.force_rescan = false;
        let dir = skins_dir.map(Path::to_path_buf);
        let cache = cache_path.map(Path::to_path_buf);
        let (tx, rx) = mpsc::channel();
        self.scan_rx = Some(rx);
        self.scan_in_flight = true;
        thread::spawn(move || {
            let result = match dir {
                Some(skins_dir) => run_cached_skin_scan(skins_dir, cache),
                None => SkinScanResult {
                    dir: None,
                    skins: Vec::new(),
                    pool: BTreeMap::new(),
                },
            };
            let _ = tx.send(result);
        });
    }

    /// Picks up a finished background scan, if any. Returns true when state
    /// changed (scan finished or worker lost) so callers can repaint.
    fn poll_scan_result(&mut self) -> bool {
        if !self.scan_in_flight {
            return false;
        }
        let result = match self.scan_rx.as_ref() {
            Some(rx) => match rx.try_recv() {
                Ok(result) => result,
                Err(mpsc::TryRecvError::Empty) => return false,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.scan_rx = None;
                    self.scan_in_flight = false;
                    return true;
                }
            },
            None => {
                self.scan_in_flight = false;
                return true;
            }
        };
        self.scan_rx = None;
        self.scan_in_flight = false;
        // Remap the selection by folder name: positional indexes shift when
        // skins are added/removed, and a fresh save names its folder.
        let previous_folder = self
            .selected
            .and_then(|index| self.skins.get(index))
            .map(|skin| skin.folder_name.clone());
        let previous_index = self.selected;
        self.scanned_dir = result.dir;
        self.skins = result.skins;
        self.pool = result.pool;
        // A rescan refreshes every skin's file list — drop a pending
        // hide confirmation rather than acting on stale paths.
        self.failsound_armed_for = None;
        let wanted = self.pending_select_folder.take().or(previous_folder);
        self.selected = wanted
            .and_then(|folder| {
                self.skins
                    .iter()
                    .position(|skin| skin.folder_name == folder)
            })
            .or(previous_index.filter(|&index| index < self.skins.len()))
            .or(if self.skins.is_empty() { None } else { Some(0) });
        // Pooled files may have vanished (skins deleted/renamed).
        self.overrides.retain(|_, entry| entry.path_exists());
        self.retain_live_imports();
        true
    }

    pub fn poll(&mut self, ctx: &egui::Context) {
        if self.poll_scan_result() {
            // A background skin scan just landed — repaint so the spinner is
            // replaced by the results without waiting for input.
            ctx.request_repaint();
        }
        while let Ok((key, result)) = self.load_rx.try_recv() {
            self.in_flight.remove(&key);
            match result {
                Ok(decoded) => {
                    let texture = ctx.load_texture(
                        format!("skin-asset:{}@{}", key.path.display(), key.max_dim),
                        decoded.image,
                        egui::TextureOptions::LINEAR,
                    );
                    self.tex.insert(
                        key.clone(),
                        LoadedTexture {
                            handle: texture,
                            game_height: decoded.game_height,
                        },
                    );
                    self.tex_order.push(key);
                    while self.tex_order.len() > TEXTURE_CACHE_LIMIT {
                        let oldest = self.tex_order.remove(0);
                        if !self.in_flight.contains(&oldest) {
                            self.tex.remove(&oldest);
                        }
                    }
                }
                Err(_) => {
                    self.failed.insert(key);
                }
            }
        }

        if self.is_saving {
            let received = self.save_rx.as_ref().map(|rx| rx.try_recv());
            match received {
                Some(Ok(result)) => {
                    self.is_saving = false;
                    self.save_rx = None;
                    self.force_rescan = true;
                    match result {
                        Ok(outcome) => {
                            self.save_ok = true;
                            // The rescan this triggers lands the new versioned
                            // folder; select it once it arrives.
                            self.pending_select_folder = Some(outcome.name.clone());
                            let mut message = format!(
                                "Saved \"{}\" — {} file(s) copied, {} element(s) replaced",
                                outcome.name, outcome.files_copied, outcome.overrides_applied
                            );
                            if outcome.resizes_applied > 0 {
                                message.push_str(&format!(", {} resized", outcome.resizes_applied));
                            }
                            message.push_str(&format!(" → {}", outcome.path.display()));
                            self.save_status = Some(message);
                        }
                        Err(err) => {
                            self.save_ok = false;
                            self.save_status = Some(format!("Save failed: {err:#}"));
                        }
                    }
                }
                Some(Err(mpsc::TryRecvError::Disconnected)) => {
                    self.is_saving = false;
                    self.save_rx = None;
                }
                Some(Err(mpsc::TryRecvError::Empty)) | None => {}
            }
        }
    }

    pub fn needs_repaint(&self) -> bool {
        !self.in_flight.is_empty() || self.is_saving || self.scan_in_flight
    }

    fn texture(&mut self, path: &Path, max_dim: u32) -> Option<LoadedTexture> {
        let key = TexKey {
            path: path.to_path_buf(),
            max_dim,
        };
        if let Some(loaded) = self.tex.get(&key) {
            return Some(loaded.clone());
        }
        if self.failed.contains(&key)
            || self.in_flight.contains(&key)
            || self.in_flight.len() >= MAX_TEXTURES_IN_FLIGHT
        {
            return None;
        }
        self.in_flight.insert(key.clone());
        let tx = self.load_tx.clone();
        thread::spawn(move || {
            let result = decode_texture(&key.path, key.max_dim).map_err(|err| format!("{err:#}"));
            let _ = tx.send((key, result));
        });
        None
    }

    fn selected_skin(&self) -> Option<&SkinSummary> {
        self.selected.and_then(|index| self.skins.get(index))
    }

    /// What the preview shows: the base skin's elements with user picks on top.
    fn preview_elements(&self) -> BTreeMap<&'static str, PathBuf> {
        let mut elements = self
            .selected_skin()
            .map(|skin| skin.elements.clone())
            .unwrap_or_default();
        for entry in self.overrides.values() {
            for (member_file, path) in &entry.files {
                elements.insert(*member_file, path.clone());
            }
        }
        elements
    }

    fn load_scene_assets(
        &mut self,
        elements: &BTreeMap<&'static str, PathBuf>,
        max_dim: u32,
        combo_colours: Vec<egui::Color32>,
        render_opts: SkinRenderOpts,
        cursor_scale: f32,
    ) -> SceneAssets {
        let mut get = |file: &str| {
            elements
                .get(file)
                .and_then(|path| self.texture(path, max_dim))
        };
        let hitcircle = get("hitcircle.png");
        let overlay = get("hitcircleoverlay.png");
        let approach = get("approachcircle.png");
        let mut assets = SceneAssets {
            cursor: get("cursor.png").map(|loaded| loaded.handle),
            trail: get("cursortrail.png").map(|loaded| loaded.handle),
            middle: get("cursormiddle.png").map(|loaded| loaded.handle),
            cursor_scale,
            hitcircle: hitcircle.as_ref().map(|loaded| loaded.handle.clone()),
            overlay: overlay.as_ref().map(|loaded| loaded.handle.clone()),
            approach: approach.as_ref().map(|loaded| loaded.handle.clone()),
            hitcircle_game_height: hitcircle.as_ref().map(|loaded| loaded.game_height),
            overlay_game_height: overlay.as_ref().map(|loaded| loaded.game_height),
            approach_game_height: approach.as_ref().map(|loaded| loaded.game_height),
            // HUD assets are only decoded while the HUD is enabled.
            bar_bg: if HUD_ENABLED {
                get("scorebar-bg.png").map(|loaded| loaded.handle)
            } else {
                None
            },
            bar_colour: if HUD_ENABLED {
                get("scorebar-colour.png").map(|loaded| loaded.handle)
            } else {
                None
            },
            bar_marker: if HUD_ENABLED {
                get("scorebar-marker.png").map(|loaded| loaded.handle)
            } else {
                None
            },
            score_x: if HUD_ENABLED {
                get("score-x.png").map(|loaded| loaded.handle)
            } else {
                None
            },
            score_comma: if HUD_ENABLED {
                get("score-comma.png").map(|loaded| loaded.handle)
            } else {
                None
            },
            score_percent: if HUD_ENABLED {
                get("score-percent.png").map(|loaded| loaded.handle)
            } else {
                None
            },
            combo_colours,
            overlay_above_number: render_opts.overlay_above_number,
            cursor_centre: render_opts.cursor_centre,
            cursor_expand: render_opts.cursor_expand,
            cursor_rotate: render_opts.cursor_rotate,
            cursortrail_rotate: render_opts.cursortrail_rotate,
            circle_digits: Default::default(),
            circle_digit_game_height: [None; 10],
            score_digits: Default::default(),
        };
        for digit in 0..10u8 {
            if let Some(loaded) = get(&format!("default-{digit}.png")) {
                assets.circle_digits[digit as usize] = Some(loaded.handle);
                assets.circle_digit_game_height[digit as usize] = Some(loaded.game_height);
            }
            assets.score_digits[digit as usize] = if HUD_ENABLED {
                get(&format!("score-{digit}.png")).map(|loaded| loaded.handle)
            } else {
                None
            };
        }
        assets
    }

    fn start_save(&mut self) {
        if self.is_saving {
            return;
        }
        let Some(base) = self.selected_skin().cloned() else {
            return;
        };
        let overrides: Vec<(String, PoolEntry)> = self
            .overrides
            .iter()
            .map(|(slot_key, entry)| ((*slot_key).to_owned(), entry.clone()))
            .collect();
        // The whole-cursor scale applies to every cursor asset; `save_skin`
        // skips slots with no file to resize and ignores the 100 % default.
        let resizes: Vec<(String, f32)> = CURSOR_SLOT_KEYS
            .iter()
            .map(|slot_key| ((*slot_key).to_owned(), self.cursor_resize))
            .collect();
        self.is_saving = true;
        self.save_status = None;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = save_skin(&base, &overrides, &resizes);
            let _ = tx.send(result);
        });
        self.save_rx = Some(rx);
    }

    // ── Sidebar ──

    pub fn sidebar(
        &mut self,
        ui: &mut egui::Ui,
        skins_dir: Option<&Path>,
        cache_path: Option<&Path>,
    ) {
        self.ensure_scanned(skins_dir, cache_path);

        match skins_dir {
            Some(dir) => scan_status(ui, "Skins folder", &dir.display().to_string()),
            None => muted_label(
                ui,
                "Set your Songs folder in the Library tab so <osu root>/Skins can be located.",
            ),
        }
        if ui
            .add_enabled(!self.scan_in_flight, egui::Button::new("⟳ Rescan skins"))
            .on_hover_text("Re-read the Skins folder and rebuild the asset pool")
            .clicked()
        {
            self.force_rescan = true;
        }
        ui.add_space(4.0);

        if self.scan_in_flight && self.skins.is_empty() {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new());
                muted_label(ui, "Scanning skins…");
            });
            return;
        }
        if self.scan_in_flight {
            // Refresh with results already on screen: keep the list
            // interactive and only note the background refresh.
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new());
                muted_label(ui, "Refreshing skins…");
            });
            ui.add_space(4.0);
        }

        let time = ui.input(|input| input.time);
        let skin_count = self.skins.len();
        for index in 0..skin_count {
            let row_selected = self.selected == Some(index);
            // Allocate the row first (sizing needs no skin data), so
            // off-screen rows skip the per-skin clones and decodes below.
            let (rect, response) = ui
                .allocate_ui(egui::vec2(ui.available_width(), 0.0), |ui| {
                    let scene_h = ((ui.available_width() - 8.0) * 9.0 / 16.0).min(150.0);
                    let scene_w = scene_h * 16.0 / 9.0;
                    ui.allocate_exact_size(egui::vec2(scene_w, scene_h), egui::Sense::click())
                })
                .inner;

            // Only pay for clones/decodes when the row is actually on screen.
            if ui.clip_rect().intersects(rect.expand(60.0)) {
                let (elements, combo_colours, render_opts) = {
                    let skin = &self.skins[index];
                    (
                        skin.elements.clone(),
                        skin.combo_colours.clone(),
                        skin.render_opts,
                    )
                };
                let assets =
                    self.load_scene_assets(&elements, THUMB_DIM, combo_colours, render_opts, 1.0);
                let painter = ui.painter_at(rect);
                draw_scene(&painter, rect, &assets, time);
                if row_selected {
                    painter.rect_stroke(
                        rect,
                        egui::Rounding::same(8.0),
                        egui::Stroke::new(2.0_f32, ACCENT),
                    );
                }
            }

            if response.clicked() {
                self.selected = Some(index);
            }
            let skin = &self.skins[index];
            ui.horizontal(|ui| {
                let name = egui::RichText::new(&skin.display_name).strong();
                ui.label(if row_selected {
                    name.color(ACCENT)
                } else {
                    name
                })
                .on_hover_text(format!(
                    "{}\n{}",
                    skin.folder_name,
                    skin.path.display()
                ));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    muted_label(
                        ui,
                        format!(
                            "{}/{} elements · {} file(s)",
                            skin.element_count(),
                            total_file_count(),
                            skin.file_count
                        ),
                    );
                });
            });
            ui.add_space(2.0);
        }
        if self.skins.is_empty() {
            muted_label(ui, "No skins found in the Skins folder yet.");
        }

        ui.add_space(6.0);
        ui.separator();
        self.save_section(ui);
    }

    fn save_section(&mut self, ui: &mut egui::Ui) {
        ui.heading("Save");
        if let Some(base) = self.selected_skin() {
            let base_label = base.folder_name.clone();
            if let Some(dir) = &self.scanned_dir {
                let target_name = next_versioned_name(&base_label, dir);
                muted_label(
                    ui,
                    format!(
                        "Will save as \"{target_name}\" — next free version of \"{base_label}\"."
                    ),
                );
                muted_label(
                    ui,
                    format!(
                        "{} slot(s) replaced from the pool; the rest is copied from the base skin.",
                        self.overrides.len()
                    ),
                );
                if (self.cursor_resize - 1.0).abs() > 1e-3 {
                    muted_label(
                        ui,
                        format!(
                            "Cursor resized to {:.0}% — artwork scaled inside the original file resolution.",
                            self.cursor_resize * 100.0
                        ),
                    );
                }
                ui.add_space(4.0);
                if self.is_saving {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new());
                        muted_label(ui, "Saving skin…");
                    });
                } else if ui
                    .button("💾 Save skin")
                    .on_hover_text(
                        "Copy the base skin to a new versioned folder with your picks applied",
                    )
                    .clicked()
                {
                    self.start_save();
                }
            } else {
                muted_label(ui, "No Skins folder set.");
            }
        } else {
            muted_label(ui, "Select a base skin above to save a remixed copy.");
        }
        if let Some(status) = &self.save_status {
            ui.add_space(4.0);
            ui.label(egui::RichText::new(status).small().color(if self.save_ok {
                egui::Color32::from_rgb(0x7f, 0xa6, 0x86)
            } else {
                egui::Color32::from_rgb(0xc2, 0x6b, 0x72)
            }));
        }
    }

    /// "Failsound" row: hiding renames the selected skin's `failsound`
    /// audio to `*.bak` in place — the game stops picking it up, nothing is
    /// deleted and no new skin version is created — while restoring renames
    /// it back. Hiding needs two clicks (arm, then confirm); restoring one.
    fn failsound_section(
        &mut self,
        ui: &mut egui::Ui,
        base_label: &str,
        base_path: &Path,
        live: &[PathBuf],
        hidden: &[PathBuf],
    ) {
        fn file_names(paths: &[PathBuf]) -> String {
            paths
                .iter()
                .filter_map(|path| path.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(", ")
        }

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Failsound").strong());
            if live.is_empty() && hidden.is_empty() {
                muted_label(ui, "none in this skin");
            } else {
                if !live.is_empty() {
                    muted_label(ui, file_names(live));
                }
                if !hidden.is_empty() {
                    muted_label(ui, format!("hidden: {}", file_names(hidden)));
                }
            }
        });
        if live.is_empty() && hidden.is_empty() {
            return;
        }
        // Stacked (not side-by-side): the preview side panel is narrow and
        // the full confirm label needs the width.
        ui.vertical(|ui| {
            if !live.is_empty() {
                let armed = self.failsound_armed_for.as_deref() == Some(base_path);
                let clicked = if armed {
                    ui.button("⚠️ Click again to confirm hide")
                        .on_hover_text(
                            "Renames the failsound files above to *.bak inside the selected \
                             skin folder. The game ignores them, nothing is deleted and no \
                             new skin version is created.",
                        )
                        .clicked()
                } else {
                    ui.button("🔇 Hide failsound")
                        .on_hover_text(
                            "Renames failsound.mp3/.ogg/.wav to *.bak inside the selected \
                             skin folder, so the fail sound stops playing. Reversible via \
                             restore — needs a second click to confirm.",
                        )
                        .clicked()
                };
                if clicked {
                    if armed {
                        self.failsound_armed_for = None;
                        match hide_failsound_files(live) {
                            Ok(count) => {
                                self.set_failsound_status(
                                    base_path,
                                    true,
                                    format!(
                                        "Hid {count} failsound file(s) from \"{base_label}\" — \
                                         restore them here anytime."
                                    ),
                                );
                                // Renames are mirrored into the skin summary
                                // below, so no rescan is needed to show them.
                                self.move_failsound_entries(base_path, true);
                            }
                            Err(err) => {
                                self.set_failsound_status(
                                    base_path,
                                    false,
                                    format!("Hide failed: {err:#}"),
                                );
                            }
                        }
                    } else {
                        self.failsound_armed_for = Some(base_path.to_path_buf());
                        self.set_failsound_status(
                            base_path,
                            false,
                            format!(
                                "Click again to confirm hiding the failsound of \"{base_label}\"."
                            ),
                        );
                    }
                }
            }
            if !hidden.is_empty()
                && ui
                    .button("🔈 Restore failsound")
                    .on_hover_text(
                        "Renames hidden *.bak failsound files back, so the game picks \
                     them up again.",
                    )
                    .clicked()
            {
                match restore_failsound_files(hidden) {
                    Ok(count) => {
                        self.set_failsound_status(
                            base_path,
                            true,
                            format!("Restored {count} failsound file(s) to \"{base_label}\"."),
                        );
                        self.move_failsound_entries(base_path, false);
                    }
                    Err(err) => {
                        self.set_failsound_status(
                            base_path,
                            false,
                            format!("Restore failed: {err:#}"),
                        );
                    }
                }
            }
        });
        // Pinned to the skin the message belongs to: selecting another skin
        // must not show the previous skin's message.
        if self.failsound_status_for.as_deref() == Some(base_path)
            && let Some(status) = &self.failsound_status
        {
            ui.label(
                egui::RichText::new(status)
                    .small()
                    .color(if self.failsound_ok {
                        egui::Color32::from_rgb(0x7f, 0xa6, 0x86)
                    } else {
                        egui::Color32::from_rgb(0xc2, 0x6b, 0x72)
                    }),
            );
        }
    }

    fn set_failsound_status(&mut self, base_path: &Path, ok: bool, message: String) {
        self.failsound_ok = ok;
        self.failsound_status = Some(message);
        self.failsound_status_for = Some(base_path.to_path_buf());
    }

    /// Mirrors a successful hide (`to_hidden`) or restore into the skin
    /// summary with the same renames `hide`/`restore_failsound_files`
    /// perform, so the lists update without a rescan.
    fn move_failsound_entries(&mut self, base_path: &Path, to_hidden: bool) {
        let Some(skin) = self.skins.iter_mut().find(|skin| skin.path == base_path) else {
            return;
        };
        if to_hidden {
            let live = std::mem::take(&mut skin.failsound);
            for path in live {
                match path.file_name().and_then(|name| name.to_str()) {
                    Some(name) => skin
                        .failsound_hidden
                        .push(path.with_file_name(format!("{name}.bak"))),
                    None => skin.failsound.push(path),
                }
            }
            skin.failsound_hidden.sort();
        } else {
            let hidden = std::mem::take(&mut skin.failsound_hidden);
            for backup in hidden {
                // ".bak" is ASCII, so slicing at the suffix boundary is safe.
                let live_name =
                    backup
                        .file_name()
                        .and_then(|name| name.to_str())
                        .and_then(|name| {
                            name.len().checked_sub(4).and_then(|len| {
                                name[len..]
                                    .eq_ignore_ascii_case(".bak")
                                    .then(|| &name[..len])
                            })
                        });
                if let Some(live_name) = live_name {
                    skin.failsound.push(backup.with_file_name(live_name));
                } else {
                    skin.failsound_hidden.push(backup);
                }
            }
            skin.failsound.sort();
        }
    }

    // ── Importing own assets ──

    /// Applies imported files: classifies them into slots by file name,
    /// pools each affected slot's files as one "Imported" entry (repeated
    /// identical imports collapse into it) and makes every import the active
    /// pick, so it shows in the preview and lands in the saved skin. The
    /// status line reports what was filed and what was skipped.
    fn import_files(&mut self, paths: Vec<PathBuf>) {
        let (placed, skipped) = classify_imports(&paths);
        let mut imported_files = 0;
        let mut labels: Vec<&'static str> = Vec::new();
        let mut touched = false;
        for (slot_key, files) in placed {
            let Some(slot) = slot_by_key(slot_key) else {
                continue;
            };
            touched = true;
            let entries = self.imported.entry(slot_key).or_default();
            let duplicate = entries.iter().find(|entry| entry.files == files).cloned();
            let entry = match duplicate {
                Some(entry) => entry,
                None => {
                    imported_files += files.len();
                    labels.push(slot.label);
                    let thumb = files
                        .get(slot.preview_file)
                        .or_else(|| files.values().next())
                        .cloned()
                        .expect("imported entry has at least one file");
                    let entry = PoolEntry {
                        skin: IMPORTED_SKIN.to_owned(),
                        files: files.clone(),
                        thumb,
                        also_in: Vec::new(),
                        is_blank: false,
                    };
                    entries.push(entry.clone());
                    entry
                }
            };
            self.overrides.insert(slot_key, entry);
        }
        self.import_ok = touched;
        self.import_status = Some(if imported_files > 0 {
            format!(
                "Imported {} file(s) into: {} — set as the active picks.{}",
                imported_files,
                labels.join(", "),
                skipped_note(&skipped),
            )
        } else if touched {
            format!(
                "Already imported — picks re-applied.{}",
                skipped_note(&skipped)
            )
        } else {
            format!(
                "Nothing imported — no element matches these file names.{}",
                skipped_note(&skipped)
            )
        });
    }

    /// Walks a folder (subfolders included) and imports every png/jpg image.
    fn import_folder(&mut self, folder: &Path) {
        match collect_files_recursive(folder) {
            // The walk yields folder-relative paths — anchor them so previews
            // and the save's `fs::copy` resolve regardless of the app's CWD.
            Ok(files) => {
                let images: Vec<PathBuf> = files
                    .into_iter()
                    .filter(|relative| is_importable_image(relative))
                    .map(|relative| folder.join(relative))
                    .collect();
                if images.is_empty() {
                    self.import_ok = false;
                    self.import_status = Some(format!(
                        "Nothing to import — no png/jpg image in {}.",
                        folder.display()
                    ));
                    return;
                }
                self.import_files(images);
            }
            Err(err) => {
                self.import_ok = false;
                self.import_status = Some(format!("Import failed: {err:#}"));
            }
        }
    }

    /// Drops imported pool entries whose source files vanished, called after
    /// every scan like the override prune (picks built on them go with the
    /// regular override prune in `poll_scan_result`).
    fn retain_live_imports(&mut self) {
        self.imported.retain(|_, entries| {
            entries.retain(|entry| entry.path_exists());
            !entries.is_empty()
        });
    }

    /// "Cursor size" block: one scale for the whole cursor — the cursor,
    /// trail and middle assets are resized together. The art is scaled
    /// inside each file's original canvas resolution (see
    /// `scale_within_canvas`), which is what changes the cursor's in-game
    /// size; applied on save.
    fn cursor_size_section(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Cursor size").strong());
        ui.vertical(|ui| {
            // Stacked (not side-by-side): the side panel is narrow.
            ui.spacing_mut().slider_width = ui.available_width().min(240.0);
            let mut scale = self.cursor_resize;
            let slider = ui.add(
                egui::Slider::new(&mut scale, 0.25..=3.0)
                    .custom_formatter(|value, _| format!("{:.0}%", value * 100.0))
                    .step_by(f64::from(RESIZE_STEP)),
            );
            if slider.changed() {
                // Exactly 100% means "no resize" — keep the field at the
                // default so the save notes stay honest.
                let on_default = (scale - 1.0).abs() < 0.005;
                // Detent on the 100 % default: a pointer drag landing on the
                // neighbouring 5 % stops (0.95/1.05) or anywhere between
                // snaps to exactly 100 %, so dropping the handle near the
                // middle never leaves a tiny accidental resize applied.
                // Keyboard steps stay exact so 95 %/105 % stay reachable.
                let dragged_to_default =
                    slider.dragged() && (scale - 1.0).abs() < RESIZE_STEP * 1.5;
                self.cursor_resize = if on_default || dragged_to_default {
                    1.0
                } else {
                    scale
                };
            }
            slider.on_hover_text(
                "Scales the whole cursor — cursor, trail and middle — inside the files without \
                 changing their pixel resolution; the game sizes elements by resolution, so \
                 this is what changes how big the cursor looks in game. Applied when the skin \
                 is saved.",
            );
            if self.cursor_resize != 1.0 {
                ui.horizontal(|ui| {
                    muted_label(
                        ui,
                        format!("{:.0}% — applied on save", self.cursor_resize * 100.0),
                    );
                    if ui
                        .small_button("↺")
                        .on_hover_text("Back to the artwork's original size")
                        .clicked()
                    {
                        self.cursor_resize = 1.0;
                    }
                });
            }
        });
        ui.add_space(4.0);
    }

    /// "Import own assets" block: pick files or a folder; everything is
    /// filed into the matching element slots by file name and becomes the
    /// active pick. The last import's status shows underneath.
    fn import_section(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Import own assets").strong());
        ui.vertical(|ui| {
            // Stacked (not side-by-side): the side panel is narrow.
            if ui
                .button("📥 Import files…")
                .on_hover_text(
                    "Pick one or more images; each is filed into the element slot its file \
                     name matches (cursor.png → Cursor, default-3.png → circle numbers, \
                     hitcircleoverlay@2x.png → hit circle overlay) and becomes the active \
                     pick for the preview and the next save.",
                )
                .clicked()
            {
                // Blocking native dialog — modal, so the UI behind it is idle.
                if let Some(files) = rfd::FileDialog::new()
                    .add_filter("Images", IMPORT_IMAGE_EXTS)
                    .pick_files()
                {
                    self.import_files(files);
                }
            }
            if ui
                .button("📂 Import folder…")
                .on_hover_text(
                    "Import every png/jpg image in a folder (subfolders included); each is \
                     filed into the element slot its file name matches.",
                )
                .clicked()
                && let Some(folder) = rfd::FileDialog::new().pick_folder()
            {
                self.import_folder(&folder);
            }
        });
        if let Some(status) = &self.import_status {
            ui.label(
                egui::RichText::new(status)
                    .small()
                    .color(if self.import_ok {
                        egui::Color32::from_rgb(0x7f, 0xa6, 0x86)
                    } else {
                        egui::Color32::from_rgb(0xc2, 0x6b, 0x72)
                    }),
            );
        }
        ui.add_space(4.0);
    }

    // ── Center panel ──

    pub fn center_page(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        self.poll(ctx);
        let content_width = ui.available_width().max(1.0);
        egui::ScrollArea::vertical()
            .id_source("skin_editor_pane")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let card_item_spacing = ui.spacing().item_spacing;
                let card_gap = 8.0;
                ui.spacing_mut().item_spacing.y = 0.0;
                set_width(ui, content_width);

                if self.skins.is_empty() {
                    if self.scan_in_flight {
                        section_frame(ctx.style().as_ref()).show(ui, |ui| {
                            ui.spacing_mut().item_spacing = card_item_spacing;
                            set_width(ui, ui.available_width());
                            ui.horizontal(|ui| {
                                ui.add(egui::Spinner::new());
                                muted_label(ui, "Scanning skins…");
                            });
                        });
                        return;
                    }
                    section_frame(ctx.style().as_ref()).show(ui, |ui| {
                        ui.spacing_mut().item_spacing = card_item_spacing;
                        set_width(ui, ui.available_width());
                        ui.heading("No skins found");
                        muted_label(
                            ui,
                            "Point the Songs folder at your osu! install (Library tab) so the \
                             Skins folder can be scanned.",
                        );
                    });
                    return;
                }

                self.preview_card(ui, card_item_spacing);
                ui.add_space(card_gap);

                for group in active_groups() {
                    egui::CollapsingHeader::new(
                        egui::RichText::new(format!("🎨 {}", group.label)).strong(),
                    )
                    .default_open(group.default_open)
                    .show(ui, |ui| {
                        set_width(ui, ui.available_width());
                        for slot in group.slots {
                            self.slot_row(ui, slot);
                        }
                    });
                    ui.add_space(card_gap);
                }
            });
    }

    fn preview_card(&mut self, ui: &mut egui::Ui, card_item_spacing: egui::Vec2) {
        let style = ui.ctx().style().clone();
        section_frame(&style).show(ui, |ui| {
            ui.spacing_mut().item_spacing = card_item_spacing;
            set_width(ui, ui.available_width());
            ui.horizontal(|ui| {
                ui.heading("Gameplay preview");
                muted_label(
                    ui,
                    "A live mock of the playfield — it updates as you switch skins or pick pooled elements.",
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let save_enabled = self.selected_skin().is_some() && !self.is_saving;
                    if ui
                        .add_enabled(save_enabled, egui::Button::new("💾 Save skin"))
                        .clicked()
                    {
                        self.start_save();
                    }
                    if ui
                        .add_enabled(
                            !self.overrides.is_empty(),
                            egui::Button::new("↺ Reset picks"),
                        )
                        .on_hover_text("Drop every pooled pick and show the base skin again")
                        .clicked()
                    {
                        self.overrides.clear();
                    }
                });
            });
            ui.horizontal(|ui| {
                if let Some(skin) = self.selected_skin() {
                    muted_label(ui, format!("Base skin: {}", skin.display_name));
                }
                let imported_count: usize = self.imported.values().map(Vec::len).sum();
                muted_label(ui, if imported_count > 0 {
                    format!(
                        "{} pooled asset(s) across {} skin(s) · {} imported",
                        self.pool.values().map(Vec::len).sum::<usize>(),
                        self.skins.len(),
                        imported_count
                    )
                } else {
                    format!(
                        "{} pooled asset(s) across {} skin(s)",
                        self.pool.values().map(Vec::len).sum::<usize>(),
                        self.skins.len()
                    )
                });
            });

            let avail_w = ui.available_width().max(120.0);
            let avail_h = (ui.available_height() * 0.55).max(230.0);
            // Room for the extras side panel (failsound hide/restore) when
            // the card is wide enough; otherwise the panel stacks below.
            const SIDE_W: f32 = 244.0;
            let gap = ui.spacing().item_spacing.x;
            let wide = avail_w - gap - SIDE_W > 360.0;
            let scene_budget_w = if wide { avail_w - gap - SIDE_W } else { avail_w };
            let mut height = scene_budget_w * 9.0 / 16.0;
            let mut width = scene_budget_w;
            if height > avail_h {
                height = avail_h;
                width = height * 16.0 / 9.0;
            }
            // Owned up front: drawing the scene and the side panel both need
            // `&mut self`, so nothing here may hold a borrow of it.
            let scene_state = {
                let elements = self.preview_elements();
                let combo_colours = self
                    .selected_skin()
                    .map(|skin| skin.combo_colours.clone())
                    .unwrap_or_default();
                let render_opts = self
                    .selected_skin()
                    .map(|skin| skin.render_opts)
                    .unwrap_or_default();
                let cursor_scale = self.cursor_resize;
                (elements, combo_colours, render_opts, cursor_scale)
            };
            let failsound = self.selected_skin().map(|skin| {
                (
                    skin.folder_name.clone(),
                    skin.path.clone(),
                    skin.failsound.clone(),
                    skin.failsound_hidden.clone(),
                )
            });
            let draw_scene_rect = |state: &mut Self, ui: &mut egui::Ui| {
                let (scene_rect, _) =
                    ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
                let (elements, combo_colours, render_opts, cursor_scale) = &scene_state;
                let assets = state.load_scene_assets(
                    elements,
                    PREVIEW_DIM,
                    combo_colours.clone(),
                    *render_opts,
                    *cursor_scale,
                );
                let time = ui.input(|input| input.time);
                let painter = ui.painter_at(scene_rect);
                draw_scene(&painter, scene_rect, &assets, time);
            };
            if wide {
                ui.horizontal(|ui| {
                    draw_scene_rect(self, ui);
                    ui.vertical(|ui| {
                        set_width(ui, ui.available_width().max(1.0));
                        ui.heading("Skin extras");
                        self.cursor_size_section(ui);
                        self.import_section(ui);
                        match &failsound {
                            Some((label, path, live, hidden)) => {
                                self.failsound_section(ui, label, path, live, hidden);
                            }
                            None => muted_label(ui, "Select a base skin in the sidebar."),
                        }
                    });
                });
            } else {
                draw_scene_rect(self, ui);
                ui.add_space(4.0);
                self.cursor_size_section(ui);
                self.import_section(ui);
                if let Some((label, path, live, hidden)) = &failsound {
                    self.failsound_section(ui, label, path, live, hidden);
                }
            }
            // Approach circle + cursor are animated.
            ui.ctx().request_repaint_after(Duration::from_millis(33));
        });
    }

    fn slot_row(&mut self, ui: &mut egui::Ui, slot: &Slot) {
        let override_entry = self.overrides.get(slot.key).cloned();
        let base_path = self.selected_skin().and_then(|skin| {
            skin.elements
                .get(slot.preview_file)
                .or_else(|| slot.files.iter().find_map(|file| skin.elements.get(file)))
                .cloned()
        });
        let current_path = match &override_entry {
            Some(entry) => entry
                .files
                .get(slot.preview_file)
                .or_else(|| entry.files.values().next())
                .cloned(),
            None => base_path.clone(),
        };
        // Own imports lead the strip; the scanned skins follow.
        let mut pool = self.imported.get(slot.key).cloned().unwrap_or_default();
        pool.extend(self.pool.get(slot.key).cloned().unwrap_or_default());

        ui.add_space(6.0);
        ui.separator();
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(slot.label).strong());
            if slot.files.len() == 1 {
                muted_label(ui, slot.files[0]);
            } else {
                muted_label(ui, format!("{} file(s) as one set", slot.files.len()));
            }
            if let Some(entry) = &override_entry {
                muted_label(ui, format!("← {} ({})", entry.skin, entry.files.len()));
                if ui
                    .small_button("↺")
                    .on_hover_text("Back to the base skin's own asset")
                    .clicked()
                {
                    self.overrides.remove(slot.key);
                }
            } else if base_path.is_none() {
                ui.label(
                    egui::RichText::new("missing — pick one from the pool")
                        .small()
                        .color(egui::Color32::from_rgb(0xc4, 0xa2, 0x6a)),
                );
            }
        });

        ui.horizontal(|ui| {
            // Current asset (what the preview uses right now).
            let (thumb_rect, thumb_response) =
                ui.allocate_exact_size(egui::vec2(56.0, 56.0), egui::Sense::hover());
            let painter = ui.painter_at(thumb_rect);
            painter.rect_filled(thumb_rect, 6.0, egui::Color32::from_rgb(0x26, 0x27, 0x2b));
            painter.rect_stroke(thumb_rect, 6.0, egui::Stroke::new(2.0_f32, ACCENT));
            if let Some(path) = &current_path
                && let Some(texture) = self.texture(path, THUMB_DIM)
            {
                // The thumb box stands in for the file's canvas: drawing the
                // art at the cursor scale shows what will actually be saved.
                let scale = if CURSOR_SLOT_KEYS.contains(&slot.key) {
                    self.cursor_resize
                } else {
                    1.0
                };
                let inner = thumb_rect.shrink(2.0);
                let fit_box = egui::Rect::from_center_size(
                    inner.center(),
                    (inner.size() * scale).max(egui::vec2(1.0, 1.0)),
                );
                draw_fitted(&painter, &texture.handle, fit_box, egui::Color32::WHITE);
            }
            thumb_response.on_hover_text(match &current_path {
                Some(path) => format!("In use: {}", path.display()),
                None => "No asset for this element yet".to_owned(),
            });

            muted_label(ui, "Pool:");

            let entries = pool.len();
            // Wrapping tile grid, not a scroller: tiles flow left-to-right
            // and continue on the next line when the row fills up.
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
                for entry in &pool {
                    // Imported tiles must match by exact content: "Imported"
                    // is every import's skin name, not an identity.
                    let picked = override_entry.as_ref().is_some_and(|pick| {
                        pick.thumb == entry.thumb
                            || (pick.skin == entry.skin && pick.skin != IMPORTED_SKIN)
                            || entry.also_in.contains(&pick.skin)
                    });
                    let (tile_rect, tile_response) =
                        ui.allocate_exact_size(egui::vec2(56.0, 56.0), egui::Sense::click());
                    let painter = ui.painter_at(tile_rect);
                    painter.rect_filled(tile_rect, 6.0, egui::Color32::from_rgb(0x26, 0x27, 0x2b));
                    let stroke_color = if picked {
                        ACCENT
                    } else if tile_response.hovered() {
                        egui::Color32::from_rgb(0x8a, 0x86, 0x80)
                    } else {
                        egui::Color32::from_rgb(0x3a, 0x3b, 0x40)
                    };
                    painter.rect_stroke(
                        tile_rect,
                        6.0,
                        egui::Stroke::new(if picked { 2.0_f32 } else { 1.0_f32 }, stroke_color),
                    );
                    if let Some(texture) = self.texture(&entry.thumb, THUMB_DIM) {
                        draw_fitted(
                            &painter,
                            &texture.handle,
                            tile_rect.shrink(4.0),
                            if entry.is_blank {
                                // Blank placeholders stay visible as tiles
                                // but draw dimmed so they read as "empty".
                                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 90)
                            } else {
                                egui::Color32::WHITE
                            },
                        );
                    }
                    // Duplicate count badge ("×3") and blank marker.
                    let copies = 1 + entry.also_in.len();
                    if copies > 1 || entry.is_blank {
                        let label = if entry.is_blank && copies > 1 {
                            format!("×{copies} blank")
                        } else if entry.is_blank {
                            "blank".to_owned()
                        } else {
                            format!("×{copies}")
                        };
                        painter.text(
                            tile_rect.right_bottom() + egui::vec2(-3.0, -2.0),
                            egui::Align2::RIGHT_BOTTOM,
                            label,
                            egui::FontId::monospace(10.0),
                            egui::Color32::from_rgb(0xc4, 0xa2, 0x6a),
                        );
                    }
                    if tile_response.clicked() {
                        self.overrides.insert(slot.key, entry.clone());
                    }
                    let summary = if entry.files.len() == 1 {
                        entry
                            .files
                            .values()
                            .next()
                            .and_then(|path| path.file_name())
                            .map_or_else(
                                || "1 file".to_owned(),
                                |name| name.to_string_lossy().to_string(),
                            )
                    } else {
                        format!("{} file(s)", entry.files.len())
                    };
                    let mut hover = if entry.skin == IMPORTED_SKIN {
                        format!("Imported asset — {summary}")
                    } else {
                        format!("{} — {summary}", entry.skin)
                    };
                    if entry.is_blank {
                        hover.push_str("\nBlank (fully transparent placeholder)");
                    }
                    if !entry.also_in.is_empty() {
                        hover.push_str(&format!(
                            "\nIdentical in {} other skin(s): {}",
                            entry.also_in.len(),
                            entry.also_in.join(", ")
                        ));
                    }
                    hover.push_str("\nClick to use in the preview and on save");
                    tile_response.on_hover_text(hover);
                }
            });
            if entries == 0 {
                muted_label(ui, "no skin ships this element");
            }
        });
        ui.add_space(4.0);
    }
}

impl PoolEntry {
    /// Any member file still on disk? Used to drop picks whose source skins
    /// were deleted or moved.
    fn path_exists(&self) -> bool {
        self.files.values().any(|path| path.exists())
    }
}

const ACCENT: egui::Color32 = egui::Color32::from_rgb(0xd8, 0x9a, 0xb0);

fn set_width(ui: &mut egui::Ui, width: f32) {
    let width = width.max(1.0);
    ui.set_width(width);
    ui.set_min_width(width);
    ui.set_max_width(width);
}

fn scan_status(ui: &mut egui::Ui, prefix: &str, value: &str) {
    let text = format!("{prefix}: {value}");
    let width = ui.available_width().max(1.0);
    ui.add_sized([width, 18.0], egui::Label::new(text.clone()).truncate(true))
        .on_hover_text(text);
}

// ── Mock gameplay scene ──────────────────────────────────────────────────────

struct SceneAssets {
    cursor: Option<egui::TextureHandle>,
    trail: Option<egui::TextureHandle>,
    middle: Option<egui::TextureHandle>,
    /// Artwork scale of the whole cursor from the editor's "Cursor size"
    /// control (1.0 = untouched): cursor, trail and middle layers all
    /// multiply their drawn size with it, so the mock shows the resize
    /// before it is saved.
    cursor_scale: f32,
    hitcircle: Option<egui::TextureHandle>,
    overlay: Option<egui::TextureHandle>,
    approach: Option<egui::TextureHandle>,
    /// Trimmed artwork heights of the circle layers in 1x game units (`None`
    /// until the layer decodes). The mock draws every layer at
    /// `height * circle / 128` so multi-layer circles keep the relative sizes
    /// the skin authored (e.g. WhiteCat's 102-unit fill inside its 118-unit
    /// ring) instead of stretching each layer to fill one shared box.
    hitcircle_game_height: Option<f32>,
    overlay_game_height: Option<f32>,
    approach_game_height: Option<f32>,
    bar_bg: Option<egui::TextureHandle>,
    bar_colour: Option<egui::TextureHandle>,
    bar_marker: Option<egui::TextureHandle>,
    circle_digits: [Option<egui::TextureHandle>; 10],
    /// Trimmed artwork height of each circle digit in 1x game units; `None`
    /// until the digit decodes.
    circle_digit_game_height: [Option<f32>; 10],
    score_digits: [Option<egui::TextureHandle>; 10],
    score_x: Option<egui::TextureHandle>,
    score_comma: Option<egui::TextureHandle>,
    score_percent: Option<egui::TextureHandle>,
    combo_colours: Vec<egui::Color32>,
    /// `HitCircleOverlayAboveNumber` (default on): the overlay covers the
    /// combo number instead of sitting behind it.
    overlay_above_number: bool,
    /// `CursorCentre` (default on): cursor art centred on the pointer
    /// (`false` = top-left origin, so the art hangs down-right of it).
    cursor_centre: bool,
    /// `CursorExpand` (default on): the cursor swells on click.
    cursor_expand: bool,
    /// `CursorRotate` (default on): the cursor spins, one clockwise
    /// revolution per 10 s (lazer's `LegacyCursor::REVOLUTION_DURATION`).
    cursor_rotate: bool,
    /// `CursorTrailRotate` (default off): trail ghosts spin with the cursor.
    cursortrail_rotate: bool,
}

/// The reference circle the game bases its elements on: `hitcircle.png` is
/// 128x128 at 1x and fills that box at the game's default circle size.
const GAME_CIRCLE_UNITS: f32 = 128.0;
/// The game downscales `default-N.png` combo numbers by 0.8x — without this
/// the preview draws every digit ~25% too large.
const DEFAULT_NUMBER_SCALE: f32 = 0.8;

impl SceneAssets {
    /// Display height for the combo number drawn on a `circle_size` circle.
    /// Always at game scale — the trimmed 1x artwork height, downscaled 0.8x
    /// like the game, scaled from the 128-unit reference circle to the mock's
    /// circle. This covers both normal digits and instafade skins (where the
    /// number sprite carries the circle art itself). Without a decoded digit
    /// there is nothing to scale, so a shrunken mock fraction stands in.
    fn circle_digit_height(&self, digit: u8, circle_size: f32) -> f32 {
        match self.circle_digit_game_height[digit as usize] {
            Some(game_height) => {
                game_scaled_height(game_height, circle_size) * DEFAULT_NUMBER_SCALE
            }
            None => circle_size * 0.4 * DEFAULT_NUMBER_SCALE,
        }
    }
}

const SCENE_W: f32 = 1024.0;
const SCENE_H: f32 = 576.0;
const UV_FULL: egui::Rect = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));

fn texture_aspect(texture: &egui::TextureHandle) -> f32 {
    let [width, height] = texture.size();
    if height == 0 {
        return 1.0;
    }
    width as f32 / height as f32
}

/// Trimmed artwork height in 1x game units, scaled from the 128-unit
/// reference circle to a `circle_size` mock circle. Every circle layer uses
/// this so the preview keeps the relative sizes the skin authored.
fn game_scaled_height(height_1x: f32, circle_size: f32) -> f32 {
    height_1x * circle_size / GAME_CIRCLE_UNITS
}

/// Display rect for a circle body layer (`hitcircle`, `hitcircleoverlay`,
/// `approachcircle`): the trimmed artwork at game scale, centred on the
/// circle. Falls back to the full circle box when no decode metadata is
/// available (should not happen for a decoded layer).
fn game_scaled_rect(
    center: egui::Pos2,
    circle_size: f32,
    texture: &egui::TextureHandle,
    game_height: Option<f32>,
) -> egui::Rect {
    match game_height {
        Some(height_1x) => {
            let height = game_scaled_height(height_1x, circle_size);
            egui::Rect::from_center_size(
                center,
                egui::vec2(height * texture_aspect(texture), height),
            )
        }
        None => egui::Rect::from_center_size(center, egui::vec2(circle_size, circle_size)),
    }
}

/// Draws a cursor-layer quad centred on `center`: `CursorRotate` spins it
/// (one clockwise revolution per 10 s, like lazer's `LegacyCursor`), which
/// needs a raw mesh since `painter.image` cannot rotate.
fn draw_cursor_quad(
    painter: &egui::Painter,
    texture: &egui::TextureHandle,
    center: egui::Pos2,
    width: f32,
    height: f32,
    angle: f32,
    tint: egui::Color32,
) {
    if angle == 0.0 {
        painter.image(
            texture.id(),
            egui::Rect::from_center_size(center, egui::vec2(width, height)),
            UV_FULL,
            tint,
        );
        return;
    }
    let (sin, cos) = angle.sin_cos();
    let (half_w, half_h) = (width * 0.5, height * 0.5);
    let corners = [
        (-half_w, -half_h),
        (half_w, -half_h),
        (half_w, half_h),
        (-half_w, half_h),
    ];
    let uvs = [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)];
    let mut mesh = egui::Mesh::with_texture(texture.id());
    for ((dx, dy), (u, v)) in corners.into_iter().zip(uvs) {
        mesh.vertices.push(egui::epaint::Vertex {
            pos: egui::pos2(
                center.x + dx * cos - dy * sin,
                center.y + dx * sin + dy * cos,
            ),
            uv: egui::pos2(u, v),
            color: tint,
        });
    }
    mesh.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
    painter.add(mesh);
}

/// Draws a texture centred in the box at its native aspect ratio — the game
/// draws elements at their native size, so nothing is ever stretched here.
fn draw_fitted(
    painter: &egui::Painter,
    texture: &egui::TextureHandle,
    box_rect: egui::Rect,
    tint: egui::Color32,
) {
    let aspect = texture_aspect(texture);
    let box_aspect = box_rect.width() / box_rect.height();
    let (width, height) = if aspect >= box_aspect {
        (box_rect.width(), box_rect.width() / aspect)
    } else {
        (box_rect.height() * aspect, box_rect.height())
    };
    painter.image(
        texture.id(),
        egui::Rect::from_center_size(box_rect.center(), egui::vec2(width, height)),
        UV_FULL,
        tint,
    );
}

/// Colour of the `combo_index`-th combo (0 = first combo on screen): the
/// game starts at Combo2 and loops back to finish on Combo1, so index into
/// `colours` (Combo1 at 0, Combo2 at 1, …) with an offset of one, wrapping
/// around. Empty lists fall back to white.
fn combo_colour_at(colours: &[egui::Color32], combo_index: usize) -> egui::Color32 {
    if colours.is_empty() {
        return egui::Color32::WHITE;
    }
    colours[(combo_index + 1) % colours.len()]
}

fn draw_scene(painter: &egui::Painter, rect: egui::Rect, assets: &SceneAssets, time: f64) {
    let scale = rect.width() / SCENE_W;
    let at = |x: f32, y: f32| rect.left_top() + egui::vec2(x * scale, y * scale);
    let dim = |value: f32| value * scale;

    // Black backdrop: the gameplay background is played at 100% dim.
    painter.rect_filled(rect, 8.0, egui::Color32::BLACK);

    // Combo colours from skin.ini drive the hit circle / approach circle
    // tints, exactly like the game: combo colours start at Combo2 and loop
    // back to finish on Combo1, so the first combo is Combo2 (index 1) and
    // the second is Combo3 (index 2), wrapping around for short lists.
    let combo_first = combo_colour_at(&assets.combo_colours, 0);
    let combo_second = combo_colour_at(&assets.combo_colours, 1);

    // Health bar (top-left, never wider than half the screen) — paused with
    // the rest of the HUD while the focus is on circles and cursor.
    if HUD_ENABLED {
        let bar_pos = at(18.0, 18.0);
        match &assets.bar_bg {
            Some(bar_bg) => {
                let aspect = texture_aspect(bar_bg);
                let mut bar_height = dim(46.0);
                let mut bar_width = bar_height * aspect;
                if bar_width > SCENE_W * 0.5 {
                    bar_width = SCENE_W * 0.5;
                    bar_height = bar_width / aspect;
                }
                let bar_rect =
                    egui::Rect::from_min_size(bar_pos, egui::vec2(bar_width, bar_height));
                painter.image(bar_bg.id(), bar_rect, UV_FULL, egui::Color32::WHITE);
                if let Some(colour) = &assets.bar_colour {
                    let fill_height = bar_height * 0.6;
                    let fill_width = (fill_height * texture_aspect(colour)).min(bar_width * 0.94);
                    let fill_pos = egui::pos2(
                        bar_rect.center().x - fill_width * 0.5,
                        bar_rect.center().y - fill_height * 0.5,
                    );
                    let health = 0.62 + 0.18 * (time * 0.9).sin() as f32;
                    let fill_rect =
                        egui::Rect::from_min_size(fill_pos, egui::vec2(fill_width, fill_height));
                    painter
                        .with_clip_rect(egui::Rect::from_min_size(
                            fill_pos,
                            egui::vec2((fill_width * health).max(1.0), fill_height),
                        ))
                        .image(colour.id(), fill_rect, UV_FULL, egui::Color32::WHITE);
                    if let Some(marker) = &assets.bar_marker {
                        let marker_height = fill_height * 0.95;
                        let marker_width = marker_height * texture_aspect(marker);
                        let marker_pos = egui::pos2(
                            fill_pos.x + fill_width * health - marker_width * 0.5,
                            fill_pos.y + (fill_height - marker_height) * 0.5,
                        );
                        painter.image(
                            marker.id(),
                            egui::Rect::from_min_size(
                                marker_pos,
                                egui::vec2(marker_width, marker_height),
                            ),
                            UV_FULL,
                            egui::Color32::WHITE,
                        );
                    }
                }
            }
            None => {
                painter.rect_filled(
                    egui::Rect::from_min_size(bar_pos, egui::vec2(dim(300.0), dim(46.0))),
                    4.0,
                    egui::Color32::from_rgba_unmultiplied(255, 255, 255, 16),
                );
            }
        }
    }

    // Score, accuracy and combo readouts — paused with the rest of the HUD.
    if HUD_ENABLED {
        // Glyph drawing with tabular cells: every glyph occupies an advance of
        // at least `min_advance` (growing to fit wide glyphs plus a fixed side
        // gap), with the glyph centred inside it. Whatever the skin's image
        // shapes are, neighbouring glyphs can never touch, and the columns
        // read like a real score display. Falls back to plain text when a skin
        // lacks the digit.
        const GLYPH_SIDE_GAP: f32 = 0.25;
        let glyph_advance =
            |texture: Option<&egui::TextureHandle>, height: f32, min_advance: f32| -> f32 {
                texture.map_or(min_advance, |texture| {
                    (height * texture_aspect(texture) + height * GLYPH_SIDE_GAP).max(min_advance)
                })
            };
        let draw_glyph = |texture: Option<&egui::TextureHandle>,
                          glyph: char,
                          top_left: egui::Pos2,
                          height: f32,
                          min_advance: f32|
         -> f32 {
            let advance = glyph_advance(texture, height, min_advance);
            match texture {
                Some(texture) => {
                    let width = height * texture_aspect(texture);
                    let x = top_left.x + (advance - width) * 0.5;
                    painter.image(
                        texture.id(),
                        egui::Rect::from_min_size(
                            egui::pos2(x, top_left.y),
                            egui::vec2(width, height),
                        ),
                        UV_FULL,
                        egui::Color32::WHITE,
                    );
                }
                None => {
                    painter.text(
                        egui::pos2(top_left.x + advance * 0.5, top_left.y + height * 0.5),
                        egui::Align2::CENTER_CENTER,
                        glyph.to_string(),
                        egui::FontId::monospace(height * 0.9),
                        egui::Color32::WHITE,
                    );
                }
            }
            advance
        };

        // Score (top right) with the accuracy readout directly beneath it.
        let score_height = dim(27.5);
        let score_min = score_height * 0.8;
        let score_glyphs = [0_u8, 1, 2, 3, 4, 5];
        let score_total: f32 = score_glyphs
            .iter()
            .map(|digit| {
                glyph_advance(
                    assets.score_digits[*digit as usize].as_ref(),
                    score_height,
                    score_min,
                )
            })
            .sum();
        let mut x = SCENE_W - 24.0 - score_total;
        for digit in score_glyphs {
            x += draw_glyph(
                assets.score_digits[digit as usize].as_ref(),
                char::from(b'0' + digit),
                egui::pos2(at(x, 0.0).x, at(0.0, 18.0).y),
                score_height,
                score_min,
            );
        }

        // Accuracy (top right, under the score): "94,37%".
        let accuracy_height = dim(20.0);
        let accuracy_min = accuracy_height * 0.8;
        let digit_tex = |digit: u8| assets.score_digits[digit as usize].as_ref();
        let accuracy_glyphs: [(Option<&egui::TextureHandle>, char, f32); 6] = [
            (digit_tex(9), '9', accuracy_min),
            (digit_tex(4), '4', accuracy_min),
            (assets.score_comma.as_ref(), ',', accuracy_height * 0.5),
            (digit_tex(3), '3', accuracy_min),
            (digit_tex(7), '7', accuracy_min),
            (assets.score_percent.as_ref(), '%', accuracy_min),
        ];
        let accuracy_total: f32 = accuracy_glyphs
            .iter()
            .map(|(texture, _, min_advance)| glyph_advance(*texture, accuracy_height, *min_advance))
            .sum();
        let mut x = SCENE_W - 24.0 - accuracy_total;
        for (texture, glyph, min_advance) in accuracy_glyphs {
            x += draw_glyph(
                texture,
                glyph,
                egui::pos2(at(x, 0.0).x, at(0.0, 18.0 + 27.5 + 12.0).y),
                accuracy_height,
                min_advance,
            );
        }

        // Combo counter (bottom left): "042x".
        let combo_height = dim(32.5);
        let combo_min = combo_height * 0.8;
        let combo_y = at(0.0, SCENE_H - 24.0 - 32.5).y;
        let mut x = 24.0;
        for digit in [0_u8, 4, 2] {
            let top_left = egui::pos2(at(x, 0.0).x, combo_y);
            x += draw_glyph(
                assets.score_digits[digit as usize].as_ref(),
                char::from(b'0' + digit),
                top_left,
                combo_height,
                combo_min,
            );
        }
        let x_height = combo_height * 0.75;
        draw_glyph(
            assets.score_x.as_ref(),
            'x',
            egui::pos2(at(x, 0.0).x, combo_y + combo_height - x_height),
            x_height,
            x_height * 0.9,
        );
    }

    // Hit circles with approach circle (combo-colour tinted).
    let circle_size = dim(118.0);
    let phase = (time % 1.6) / 1.6;
    draw_circle(
        painter,
        assets,
        at(588.0, 318.0),
        circle_size,
        Some(2),
        Some(phase as f32),
        combo_first,
    );
    draw_circle(
        painter,
        assets,
        at(742.0, 236.0),
        circle_size,
        Some(4),
        None,
        combo_second,
    );

    // Cursor with trail, honouring the skin.ini `[General]` cursor switches:
    // `CursorCentre` (top-left origin hangs the art down-right of the
    // pointer), `CursorRotate` (clockwise spin) and `CursorExpand` (a swell
    // on each mock click, synced to the approach landing). Only `cursor.png`
    // expands, like lazer's `LegacyCursor` expand target.
    let cursor_pos = |t: f64| {
        at(
            512.0 + (t * 1.1).sin() as f32 * 110.0,
            330.0 + (t * 0.83).cos() as f32 * 70.0,
        )
    };
    let cursor_spin = if assets.cursor_rotate {
        time as f32 * std::f32::consts::TAU / 10.0
    } else {
        0.0
    };
    let click_age = time % 1.6;
    let cursor_expand = if assets.cursor_expand {
        if click_age < 0.1 {
            1.0 + 0.3 * (click_age / 0.1) as f32
        } else if click_age < 0.2 {
            1.3 - 0.3 * ((click_age - 0.1) / 0.1) as f32
        } else {
            1.0
        }
    } else {
        1.0
    };
    // Top-left origin, if requested: shift the quad so its top-left corner
    // lands on the pointer instead of its centre.
    let origin_shift = |width: f32, height: f32| {
        if assets.cursor_centre {
            egui::vec2(0.0, 0.0)
        } else {
            egui::vec2(width * 0.5, height * 0.5)
        }
    };
    let trail_texture = assets.trail.as_ref().or(assets.cursor.as_ref());
    for step in (1..=6).rev() {
        let Some(texture) = trail_texture else {
            break;
        };
        let alpha = (1.0 - step as f32 / 7.0) * 0.85;
        let height = dim(if assets.trail.is_some() { 40.0 } else { 36.0 }) * assets.cursor_scale;
        let width = height * texture_aspect(texture);
        draw_cursor_quad(
            painter,
            texture,
            cursor_pos(time - 0.05 * step as f64) + origin_shift(width, height),
            width,
            height,
            if assets.cursortrail_rotate {
                cursor_spin
            } else {
                0.0
            },
            egui::Color32::WHITE.gamma_multiply(alpha),
        );
    }
    if let Some(middle) = &assets.middle {
        let height = dim(30.0) * assets.cursor_scale;
        let width = height * texture_aspect(middle);
        draw_cursor_quad(
            painter,
            middle,
            cursor_pos(time) + origin_shift(width, height),
            width,
            height,
            0.0,
            egui::Color32::WHITE,
        );
    }
    if let Some(cursor) = &assets.cursor {
        let height = dim(46.0) * assets.cursor_scale * cursor_expand;
        let width = height * texture_aspect(cursor);
        draw_cursor_quad(
            painter,
            cursor,
            cursor_pos(time) + origin_shift(width, height),
            width,
            height,
            cursor_spin,
            egui::Color32::WHITE,
        );
    }
}

fn draw_circle(
    painter: &egui::Painter,
    assets: &SceneAssets,
    center: egui::Pos2,
    size: f32,
    number: Option<u8>,
    approach_phase: Option<f32>,
    tint: egui::Color32,
) {
    let circle_box = egui::Rect::from_center_size(center, egui::vec2(size, size));
    // Body and overlay are drawn at their own game sizes, not fitted into one
    // shared box: skins author them at different diameters (WhiteCat's fill
    // is 102 units inside a 118-unit ring) and the game scales both by the
    // same factor, so the inner edge stays visible exactly like in game.
    match &assets.hitcircle {
        Some(texture) => {
            let rect = game_scaled_rect(center, size, texture, assets.hitcircle_game_height);
            painter.image(texture.id(), rect, UV_FULL, tint);
        }
        None => {
            let fill = egui::Color32::from_rgba_unmultiplied(tint.r(), tint.g(), tint.b(), 60);
            painter.rect_filled(circle_box, 8.0, fill);
        }
    };
    if let Some(phase) = approach_phase {
        // Close-down multiplier on top of the approach art's own game size
        // (approach files are authored larger than the circle — 126+ units).
        let anim = 1.15 + 2.2 * (1.0 - phase);
        let outer = match assets.approach_game_height {
            Some(art_1x) => {
                let art = assets
                    .approach
                    .as_ref()
                    .map(|texture| art_1x.max(art_1x * texture_aspect(texture)))
                    .unwrap_or(art_1x);
                game_scaled_height(art, size) * anim
            }
            None => size * anim,
        };
        let approach_box = egui::Rect::from_center_size(center, egui::vec2(outer, outer));
        match &assets.approach {
            Some(texture) => draw_fitted(painter, texture, approach_box, tint),
            None => {
                painter.circle_stroke(center, outer * 0.5, egui::Stroke::new(2.0_f32, tint));
            }
        };
    }
    // `HitCircleOverlayAboveNumber` (default on): the overlay covers the
    // combo number; otherwise it sits behind it like WhiteCat's.
    let draw_overlay = |painter: &egui::Painter| {
        if let Some(overlay) = &assets.overlay {
            // The overlay is not tinted in game — keep it at full colour.
            let rect = game_scaled_rect(center, size, overlay, assets.overlay_game_height);
            painter.image(overlay.id(), rect, UV_FULL, egui::Color32::WHITE);
        }
    };
    let draw_number = |painter: &egui::Painter| {
        let Some(digit) = number else {
            return;
        };
        let height = assets.circle_digit_height(digit, size);
        match assets.circle_digits[digit as usize].as_ref() {
            Some(texture) => {
                let width = height * texture_aspect(texture);
                painter.image(
                    texture.id(),
                    egui::Rect::from_center_size(center, egui::vec2(width, height)),
                    UV_FULL,
                    egui::Color32::WHITE,
                );
            }
            None => {
                painter.text(
                    center,
                    egui::Align2::CENTER_CENTER,
                    digit.to_string(),
                    egui::FontId::proportional(height * 0.8),
                    egui::Color32::WHITE,
                );
            }
        }
    };
    if assets.overlay_above_number {
        draw_number(painter);
        draw_overlay(painter);
    } else {
        draw_overlay(painter);
        draw_number(painter);
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn slot_matching_covers_variants() {
        assert!(file_matches_slot("cursor.png", "cursor.png"));
        assert!(file_matches_slot("cursor@2x.png", "cursor.png"));
        assert!(file_matches_slot("CURSOR.PNG", "cursor.png"));
        assert!(file_matches_slot(
            "menu-background.jpg",
            "menu-background.jpg"
        ));
        assert!(file_matches_slot(
            "menu-background.jpeg",
            "menu-background.jpg"
        ));
        assert!(!file_matches_slot("cursortrail.png", "cursor.png"));
        assert!(!file_matches_slot("cursor.gif", "cursor.png"));
        assert!(!file_matches_slot("notcursor.png", "cursor.png"));
    }

    #[test]
    fn version_names_skip_existing_folders() {
        let dir = unique_temp_dir("osu-skin-version");
        assert_eq!(next_versioned_name("Cool", &dir), "Cool v1");
        fs::create_dir_all(dir.join("Cool v1")).unwrap();
        assert_eq!(next_versioned_name("Cool", &dir), "Cool v2");
        // Editing a versioned skin continues the lineage.
        assert_eq!(next_versioned_name("Cool v2", &dir), "Cool v3");
        // Names without a parseable version get a fresh v1.
        assert_eq!(next_versioned_name("Cool v2b", &dir), "Cool v2b v1");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn skin_ini_patching_keeps_everything_else() {
        let original = "[General]\nName: Old\nAuthor: Someone\nCursorSize: 1\n\n[Colours]\nCombo1: 255,128,0\n";
        let patched = patch_skin_ini(Some(original), "Old v2", Some("Someone"));
        assert!(patched.contains("Name: Old v2"));
        assert!(patched.contains("Author: Someone"));
        assert!(patched.contains("CursorSize: 1"));
        assert!(patched.contains("Combo1: 255,128,0"));
        // No [General]: a minimal header is prepended.
        let bare = patch_skin_ini(None, "New", None);
        assert!(bare.starts_with("[General]\nName: New\nAuthor: \n"));
    }

    #[test]
    fn skin_ini_combo_colours_skip_commented_lines() {
        let dir = unique_temp_dir("osu-skin-colours");
        let ini = dir.join("skin.ini");
        fs::write(
            &ini,
            "[General]\nName: X\n\n[Colours]\n// Combo1: 1, 2, 3\n; Combo2: 9, 9, 9\n\
             Combo1: 255, 128, 0\nCombo2: 10,20,30\nCombo3: 1.0, 0.5, 0\n\
             Combo4: 200, 100, 50 // trailing comment\nComboBad: a, b, c\n",
        )
        .unwrap();
        let meta = read_skin_meta(&ini);
        assert_eq!(meta.name.as_deref(), Some("X"));
        assert_eq!(meta.combo_colours.len(), 4);
        assert_eq!(meta.combo_colours[0], egui::Color32::from_rgb(255, 128, 0));
        assert_eq!(meta.combo_colours[1], egui::Color32::from_rgb(10, 20, 30));
        // 0–1 floats scale up to the 0–255 range.
        assert_eq!(meta.combo_colours[2], egui::Color32::from_rgb(255, 128, 0));
        assert_eq!(meta.combo_colours[3], egui::Color32::from_rgb(200, 100, 50));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn skin_ini_combo_colours_sort_by_number() {
        let dir = unique_temp_dir("osu-skin-colours-order");
        let ini = dir.join("skin.ini");
        fs::write(
            &ini,
            "[Colours]\nCombo3: 3, 3, 3\nCombo1: 1, 1, 1\nCombo2: 2, 2, 2\n",
        )
        .unwrap();
        let meta = read_skin_meta(&ini);
        assert_eq!(
            meta.combo_colours,
            vec![
                egui::Color32::from_rgb(1, 1, 1),
                egui::Color32::from_rgb(2, 2, 2),
                egui::Color32::from_rgb(3, 3, 3),
            ]
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn preview_uses_combo2_then_combo3() {
        let colours = vec![
            egui::Color32::from_rgb(1, 0, 0), // Combo1 — last in game
            egui::Color32::from_rgb(0, 2, 0), // Combo2 — first in game
            egui::Color32::from_rgb(0, 0, 3), // Combo3 — second in game
        ];
        assert_eq!(combo_colour_at(&colours, 0), colours[1]);
        assert_eq!(combo_colour_at(&colours, 1), colours[2]);
        assert_eq!(combo_colour_at(&colours, 2), colours[0]);
        // Short lists wrap: Combo2 then Combo1.
        let two = vec![
            egui::Color32::from_rgb(1, 0, 0),
            egui::Color32::from_rgb(0, 2, 0),
        ];
        assert_eq!(combo_colour_at(&two, 0), two[1]);
        assert_eq!(combo_colour_at(&two, 1), two[0]);
        // A lone Combo1 tints everything; empty falls back to white.
        let one = vec![egui::Color32::from_rgb(9, 9, 9)];
        assert_eq!(combo_colour_at(&one, 0), one[0]);
        assert_eq!(combo_colour_at(&one, 1), one[0]);
        assert_eq!(combo_colour_at(&[], 0), egui::Color32::WHITE);
    }

    #[test]
    fn skin_ini_reads_utf16_files() {
        let dir = unique_temp_dir("osu-skin-utf16");
        let ini = dir.join("skin.ini");
        let text = "[General]\nName: W\nAuthor: V\n\n[Colours]\nCombo1: 255, 128, 0\n";
        let mut bytes: Vec<u8> = vec![0xFF, 0xFE];
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        fs::write(&ini, bytes).unwrap();

        let meta = read_skin_meta(&ini);
        assert_eq!(meta.name.as_deref(), Some("W"));
        assert_eq!(meta.author.as_deref(), Some("V"));
        assert_eq!(
            meta.combo_colours,
            vec![egui::Color32::from_rgb(255, 128, 0)]
        );
        let _ = fs::remove_dir_all(dir);
    }

    fn write_png(path: &Path, color: [u8; 4]) {
        image::RgbaImage::from_pixel(16, 16, image::Rgba(color))
            .save(path)
            .unwrap();
    }

    #[test]
    fn scan_and_pool_collect_elements_across_skins() {
        let dir = unique_temp_dir("osu-skin-scan");
        let skins = dir.join("Skins");
        let alpha = skins.join("Alpha");
        let beta = skins.join("Beta");
        fs::create_dir_all(&alpha).unwrap();
        fs::create_dir_all(&beta).unwrap();
        fs::write(
            alpha.join("skin.ini"),
            "[General]\nName: Alpha Skin\nAuthor: A\n",
        )
        .unwrap();
        write_png(&alpha.join("cursor.png"), [255, 0, 0, 255]);
        write_png(&alpha.join("hitcircle.png"), [0, 255, 0, 255]);
        write_png(&beta.join("cursor@2x.png"), [0, 0, 255, 255]);
        write_png(&beta.join("approachcircle.png"), [255, 255, 0, 255]);

        let scanned = scan_skins(&skins);
        assert_eq!(scanned.len(), 2);
        let alpha_skin = scanned.iter().find(|s| s.folder_name == "Alpha").unwrap();
        assert_eq!(alpha_skin.display_name, "Alpha Skin");
        assert_eq!(alpha_skin.author.as_deref(), Some("A"));
        assert_eq!(alpha_skin.element_count(), 2);

        let pool = build_pool(&scanned);
        let cursor_pool = &pool["cursor"];
        assert_eq!(cursor_pool.len(), 2);
        let beta_entry = cursor_pool
            .iter()
            .find(|entry| entry.skin == "Beta")
            .unwrap();
        // The @2x file is the one the game (and the editor) would use.
        assert_eq!(
            beta_entry
                .files
                .get("cursor.png")
                .map(|p| p.file_name().and_then(|n| n.to_str()).unwrap_or("")),
            Some("cursor@2x.png")
        );
        assert_eq!(
            beta_entry.thumb.file_name().and_then(|n| n.to_str()),
            Some("cursor@2x.png")
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn cached_scan_reuses_unchanged_skins_across_runs() {
        let dir = unique_temp_dir("osu-skin-cache");
        let skins = dir.join("Skins");
        let alpha = skins.join("Alpha");
        fs::create_dir_all(&alpha).unwrap();
        fs::write(
            alpha.join("skin.ini"),
            "[General]\nName: Alpha Skin\nAuthor: A\n",
        )
        .unwrap();
        write_png(&alpha.join("cursor.png"), [255, 0, 0, 255]);
        write_png(&alpha.join("hitcircle.png"), [0, 255, 0, 255]);
        let cache_path = dir.join("skin_cache.json");

        // Cold cache: scans, decodes and writes the cache file.
        let first = run_cached_skin_scan(skins.clone(), Some(cache_path.clone()));
        assert_eq!(first.skins.len(), 1);
        assert!(cache_path.exists(), "scan writes the cache file");
        let pool_tiles: usize = first.pool.values().map(Vec::len).sum();

        // Nothing changed: same summaries and pool tiles, served from cache.
        let second = run_cached_skin_scan(skins.clone(), Some(cache_path.clone()));
        assert_eq!(second.skins.len(), 1);
        assert_eq!(second.skins[0].display_name, "Alpha Skin");
        assert_eq!(second.skins[0].author.as_deref(), Some("A"));
        assert_eq!(second.skins[0].elements, first.skins[0].elements);
        assert_eq!(
            second.pool.values().map(Vec::len).sum::<usize>(),
            pool_tiles
        );

        // Change a skin file (new size → new fingerprint): the skin is
        // re-scanned and the cache updated, everything else reused.
        image::RgbaImage::from_pixel(32, 32, image::Rgba([0, 255, 0, 255]))
            .save(alpha.join("hitcircle.png"))
            .unwrap();
        let third = run_cached_skin_scan(skins.clone(), Some(cache_path));
        assert_eq!(third.skins.len(), 1);
        assert_eq!(third.skins[0].elements.len(), first.skins[0].elements.len());
        assert!(
            third.skins[0]
                .elements
                .get("hitcircle.png")
                .is_some_and(|path| path.exists())
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn failsound_hide_restore_updates_lists_in_place() {
        let dir = unique_temp_dir("osu-skin-failsound-state");
        let skins = dir.join("Skins");
        let alpha = skins.join("Alpha");
        fs::create_dir_all(&alpha).unwrap();
        let live = alpha.join("failsound.mp3");
        fs::write(&live, b"fake-audio").unwrap();

        let mut state = SkinEditorState::new();
        state.skins = scan_skins(&skins);
        assert_eq!(state.skins.len(), 1);
        let root = state.skins[0].path.clone();
        assert_eq!(state.skins[0].failsound, vec![live.clone()]);

        // Successful hide mirrors the rename without a rescan …
        let hidden = live.with_file_name("failsound.mp3.bak");
        fs::rename(&live, &hidden).unwrap();
        state.move_failsound_entries(&root, true);
        assert!(state.skins[0].failsound.is_empty());
        assert_eq!(state.skins[0].failsound_hidden, vec![hidden.clone()]);

        // … and restore moves it back.
        fs::rename(&hidden, &live).unwrap();
        state.move_failsound_entries(&root, false);
        assert_eq!(state.skins[0].failsound, vec![live]);
        assert!(state.skins[0].failsound_hidden.is_empty());

        // Status messages are pinned to their skin so selecting another
        // skin never shows the previous skin's message.
        state.set_failsound_status(&root, true, "done".to_owned());
        assert_eq!(state.failsound_status_for.as_deref(), Some(root.as_path()));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn pool_dedupes_pixel_identical_art() {
        let dir = unique_temp_dir("osu-skin-dedup");
        let skins = dir.join("Skins");
        for name in ["Alpha", "Beta", "Gamma"] {
            fs::create_dir_all(skins.join(name)).unwrap();
        }
        // Alpha and Beta share the exact same cursor art; Gamma differs.
        write_png(&skins.join("Alpha").join("cursor.png"), [10, 20, 30, 255]);
        write_png(&skins.join("Beta").join("cursor.png"), [10, 20, 30, 255]);
        write_png(&skins.join("Gamma").join("cursor.png"), [200, 10, 10, 255]);

        let scanned = scan_skins(&skins);
        let pool = build_pool(&scanned);
        let cursor_pool = &pool["cursor"];
        assert_eq!(cursor_pool.len(), 2, "identical cursors collapse");
        let merged = cursor_pool
            .iter()
            .find(|entry| entry.skin == "Alpha")
            .expect("first skin stays representative");
        assert_eq!(merged.also_in, vec!["Beta".to_owned()]);
        assert!(!merged.is_blank);
        assert!(cursor_pool.iter().any(|entry| entry.skin == "Gamma"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn pool_collapses_blank_placeholders_and_sorts_them_last() {
        let dir = unique_temp_dir("osu-skin-blank");
        let skins = dir.join("Skins");
        for name in ["Real", "BlankA", "BlankB"] {
            fs::create_dir_all(skins.join(name)).unwrap();
        }
        write_png(
            &skins.join("Real").join("cursortrail.png"),
            [255, 255, 255, 255],
        );
        // Different canvas sizes, both fully transparent — same blank tile.
        write_png(&skins.join("BlankA").join("cursortrail.png"), [0, 0, 0, 0]);
        image::RgbaImage::from_pixel(1, 1, image::Rgba([0, 0, 0, 0]))
            .save(skins.join("BlankB").join("cursortrail.png"))
            .unwrap();

        let scanned = scan_skins(&skins);
        let pool = build_pool(&scanned);
        let trail_pool = &pool["cursortrail"];
        assert_eq!(trail_pool.len(), 2, "both blanks collapse into one tile");
        // Real art first, blank last.
        assert!(!trail_pool[0].is_blank);
        assert_eq!(trail_pool[0].skin, "Real");
        assert!(trail_pool[1].is_blank);
        assert_eq!(trail_pool[1].also_in.len(), 1);
        assert!(
            trail_pool[1].skin == "BlankA" || trail_pool[1].skin == "BlankB",
            "one blank stays representative, the other folds in"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn pool_keeps_partial_sets_separate() {
        let dir = unique_temp_dir("osu-skin-partial");
        let skins = dir.join("Skins");
        let full = skins.join("Full");
        let partial = skins.join("Partial");
        fs::create_dir_all(&full).unwrap();
        fs::create_dir_all(&partial).unwrap();
        for digit in 0..10u8 {
            write_png(&full.join(format!("default-{digit}.png")), [1, 2, 3, 255]);
            if digit < 5 {
                write_png(
                    &partial.join(format!("default-{digit}.png")),
                    [1, 2, 3, 255],
                );
            }
        }

        let scanned = scan_skins(&skins);
        let pool = build_pool(&scanned);
        let numbers = &pool["circle-numbers"];
        assert_eq!(
            numbers.len(),
            2,
            "same art but different coverage must not collapse"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn warm_visual_keys_matches_sequential_pool_build() {
        let dir = unique_temp_dir("osu-skin-warm");
        let skins = dir.join("Skins");
        for (name, cursor_colour) in [
            ("Alpha", [255u8, 0, 0, 255]),
            ("Beta", [255, 0, 0, 255]),
            ("Gamma", [0, 0, 255, 255]),
        ] {
            let root = skins.join(name);
            fs::create_dir_all(&root).unwrap();
            write_png(&root.join("cursor.png"), cursor_colour);
            write_png(&root.join("hitcircle.png"), [10, 20, 30, 255]);
            write_png(&root.join("default-0.png"), [40, 50, 60, 255]);
        }
        let scanned = scan_skins(&skins);

        let mut key_cache = VisualKeyCache::new();
        warm_visual_keys(&scanned, &mut key_cache);
        // Every element got a stat-valid entry, so the sequential pool
        // build below has nothing left to decode.
        for skin in &scanned {
            for path in skin.elements.values() {
                let entry = key_cache
                    .get(path)
                    .unwrap_or_else(|| panic!("no key for {}", path.display()));
                assert_eq!(entry.fingerprint, stat_fingerprint(path).unwrap());
            }
        }
        let warmed = build_pool_with_keys(&scanned, &mut key_cache);

        let sequential = build_pool(&scanned);
        let shape = |pool: &BTreeMap<&'static str, Vec<PoolEntry>>| {
            pool.iter()
                .map(|(slot, entries)| {
                    (
                        *slot,
                        entries
                            .iter()
                            .map(|entry| {
                                (
                                    entry.skin.clone(),
                                    entry.also_in.clone(),
                                    entry.is_blank,
                                    entry
                                        .thumb
                                        .file_name()
                                        .map(|name| name.to_string_lossy().into_owned()),
                                )
                            })
                            .collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(shape(&sequential), shape(&warmed));

        // A stat-valid entry is trusted as-is: warm-up must not re-decode
        // the file just because the key looks wrong.
        let cursor = scanned[0].elements.get("cursor.png").unwrap();
        key_cache.insert(
            cursor.clone(),
            VisualKeyEntry {
                fingerprint: stat_fingerprint(cursor).unwrap(),
                key: "sentinel".to_owned(),
                is_blank: false,
            },
        );
        warm_visual_keys(&scanned, &mut key_cache);
        assert_eq!(
            key_cache.get(cursor).map(|entry| entry.key.as_str()),
            Some("sentinel")
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn scan_splits_live_and_hidden_failsound() {
        let dir = unique_temp_dir("osu-skin-failsound-scan");
        let skins = dir.join("Skins");
        let skin = skins.join("Noisy");
        fs::create_dir_all(&skin).unwrap();
        fs::create_dir_all(skin.join("sub")).unwrap();
        for name in [
            "failsound.mp3",
            "FAILSOUND.OGG",
            "failsound.wav",
            // Lookalikes that must be left alone.
            "failsound.png",
            "failsound.mp4",
            "applause.mp3",
            "myfailsound.mp3",
        ] {
            fs::write(skin.join(name), b"data").unwrap();
        }
        // Nested failsound is not loaded by the game — never listed.
        fs::write(skin.join("sub").join("failsound.mp3"), b"data").unwrap();
        // Hidden backups, incl. an uppercase suffix and a non-audio namesake.
        for name in [
            "failsound.mp3.bak",
            "FAILSOUND.WAV.BAK",
            "failsound.png.bak",
        ] {
            fs::write(skin.join(name), b"data").unwrap();
        }

        let scanned = scan_skins(&skins);
        let noisy = scanned.iter().find(|s| s.folder_name == "Noisy").unwrap();
        let mut live: Vec<String> = noisy
            .failsound
            .iter()
            .filter_map(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .collect();
        live.sort();
        assert_eq!(
            live,
            vec!["FAILSOUND.OGG", "failsound.mp3", "failsound.wav"]
        );
        let mut hidden: Vec<String> = noisy
            .failsound_hidden
            .iter()
            .filter_map(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .collect();
        hidden.sort();
        assert_eq!(hidden, vec!["FAILSOUND.WAV.BAK", "failsound.mp3.bak"]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn hide_and_restore_failsound_round_trip() {
        let dir = unique_temp_dir("osu-skin-failsound-hide");
        let skin = dir.join("Noisy");
        fs::create_dir_all(&skin).unwrap();
        fs::write(skin.join("failsound.mp3"), b"loud").unwrap();
        fs::write(skin.join("failsound.ogg"), b"loud").unwrap();
        fs::write(skin.join("cursor.png"), b"data").unwrap();

        // Hiding renames in place — nothing is deleted, nothing else moves.
        let live = vec![skin.join("failsound.mp3"), skin.join("failsound.ogg")];
        assert_eq!(hide_failsound_files(&live).unwrap(), 2);
        assert!(!skin.join("failsound.mp3").exists());
        assert!(!skin.join("failsound.ogg").exists());
        assert!(skin.join("failsound.mp3.bak").exists());
        assert!(skin.join("failsound.ogg.bak").exists());
        assert!(skin.join("cursor.png").exists(), "other files are kept");
        // Hiding an empty list is a no-op, not an error.
        assert_eq!(hide_failsound_files(&[]).unwrap(), 0);

        // Restoring renames back; backups disappear.
        let hidden = vec![
            skin.join("failsound.mp3.bak"),
            skin.join("failsound.ogg.bak"),
        ];
        assert_eq!(restore_failsound_files(&hidden).unwrap(), 2);
        assert_eq!(fs::read(skin.join("failsound.mp3")).unwrap(), b"loud");
        assert!(!skin.join("failsound.mp3.bak").exists());

        // Restore refuses to overwrite a live file of the same name.
        assert_eq!(
            hide_failsound_files(&[skin.join("failsound.mp3")]).unwrap(),
            1
        );
        fs::write(skin.join("failsound.mp3"), b"replacement").unwrap();
        assert!(
            restore_failsound_files(&[skin.join("failsound.mp3.bak")]).is_err(),
            "live file must not be overwritten"
        );
        assert_eq!(
            fs::read(skin.join("failsound.mp3")).unwrap(),
            b"replacement"
        );
        assert!(skin.join("failsound.mp3.bak").exists());

        // Hiding again replaces the stale backup with the current file.
        assert_eq!(
            hide_failsound_files(&[skin.join("failsound.mp3")]).unwrap(),
            1
        );
        assert_eq!(
            fs::read(skin.join("failsound.mp3.bak")).unwrap(),
            b"replacement"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn skin_ini_render_switches_default_on() {
        let dir = unique_temp_dir("osu-skin-render-default");
        let ini = dir.join("skin.ini");
        fs::write(&ini, "[General]\nName: X\n").unwrap();
        let meta = read_skin_meta(&ini);
        assert!(meta.render_opts.overlay_above_number);
        assert!(meta.render_opts.cursor_centre);
        assert!(meta.render_opts.cursor_expand);
        assert!(meta.render_opts.cursor_rotate);
        assert!(!meta.render_opts.cursortrail_rotate);
        assert_eq!(meta.hitcircle_prefix, None);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn skin_ini_render_switches_parse_and_typo() {
        let dir = unique_temp_dir("osu-skin-render-opts");
        let ini = dir.join("skin.ini");
        fs::write(
            &ini,
            "[General]\nName: X\nCursorCentre: 0\nCursorExpand: 0\nCursorRotate: 0\n\
             CursorTrailRotate: 1\nHitCircleOverlayAboveNumer: 0\n\n\
             [Fonts]\nHitCirclePrefix: Assets/default/default\n",
        )
        .unwrap();
        let meta = read_skin_meta(&ini);
        // The legacy typo is honoured like in game.
        assert!(!meta.render_opts.overlay_above_number);
        assert!(!meta.render_opts.cursor_centre);
        assert!(!meta.render_opts.cursor_expand);
        assert!(!meta.render_opts.cursor_rotate);
        assert!(meta.render_opts.cursortrail_rotate);
        assert_eq!(
            meta.hitcircle_prefix.as_deref(),
            Some("Assets/default/default")
        );
        // Correct spelling wins over the typo when both are present.
        fs::write(
            &ini,
            "[General]\nHitCircleOverlayAboveNumer: 0\nHitCircleOverlayAboveNumber: 1\n",
        )
        .unwrap();
        assert!(read_skin_meta(&ini).render_opts.overlay_above_number);
        // Only `1`/`0` count — garbage never flips a switch.
        fs::write(
            &ini,
            "[General]\nCursorExpand: 2\nHitCircleOverlayAboveNumber: yes\n",
        )
        .unwrap();
        let meta = read_skin_meta(&ini);
        assert!(meta.render_opts.cursor_expand);
        assert!(meta.render_opts.overlay_above_number);
        // Prefixes escaping the skin folder are rejected.
        fs::write(&ini, "[Fonts]\nHitCirclePrefix: ../../evil\n").unwrap();
        assert_eq!(read_skin_meta(&ini).hitcircle_prefix, None);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn scan_resolves_prefixed_digits() {
        let dir = unique_temp_dir("osu-skin-prefix");
        let skins = dir.join("Skins");
        // WhiteCat layout: digits only under Assets/default/, named by the
        // [Fonts] HitCirclePrefix.
        let white = skins.join("White");
        let nested = white.join("Assets").join("default");
        fs::create_dir_all(&nested).unwrap();
        fs::write(
            white.join("skin.ini"),
            "[General]\nName: White\n\n[Fonts]\nHitCirclePrefix: Assets/default/default\n",
        )
        .unwrap();
        write_png(&nested.join("default-2@2x.png"), [255, 255, 255, 255]);
        write_png(&nested.join("default-3.png"), [255, 255, 255, 255]);
        // Custom basename prefix in another skin.
        let custom = skins.join("Custom");
        fs::create_dir_all(custom.join("fx")).unwrap();
        fs::write(
            custom.join("skin.ini"),
            "[General]\nName: Custom\n\n[Fonts]\nHitCirclePrefix: fx/hit\n",
        )
        .unwrap();
        write_png(
            &custom.join("fx").join("hit-4@2x.png"),
            [255, 255, 255, 255],
        );
        // No prefix: plain root digits still resolve the legacy way.
        let plain = skins.join("Plain");
        fs::create_dir_all(&plain).unwrap();
        write_png(&plain.join("default-5.png"), [255, 255, 255, 255]);

        let posix = |path: &Path| path.to_string_lossy().replace('\\', "/");
        let scanned = scan_skins(&skins);
        let white = scanned.iter().find(|s| s.folder_name == "White").unwrap();
        assert!(
            posix(&white.elements["default-2.png"]).ends_with("Assets/default/default-2@2x.png")
        );
        assert!(posix(&white.elements["default-3.png"]).ends_with("Assets/default/default-3.png"));
        let custom = scanned.iter().find(|s| s.folder_name == "Custom").unwrap();
        assert!(posix(&custom.elements["default-4.png"]).ends_with("fx/hit-4@2x.png"));
        let plain = scanned.iter().find(|s| s.folder_name == "Plain").unwrap();
        assert_eq!(
            plain.elements["default-5.png"]
                .file_name()
                .and_then(|n| n.to_str()),
            Some("default-5.png")
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn save_copies_base_applies_picks_and_bumps_version() {
        let dir = unique_temp_dir("osu-skin-save");
        let skins = dir.join("Skins");
        let base = skins.join("Base");
        let donor = skins.join("Donor");
        fs::create_dir_all(&base).unwrap();
        fs::create_dir_all(&donor).unwrap();
        fs::write(base.join("skin.ini"), "[General]\nName: Base\nAuthor: Me\n").unwrap();
        write_png(&base.join("cursor.png"), [255, 0, 0, 255]);
        write_png(&base.join("hitcircle@2x.png"), [0, 255, 0, 255]);
        write_png(&donor.join("cursor.png"), [0, 0, 255, 255]);

        let scanned = scan_skins(&skins);
        let base_skin = scanned
            .iter()
            .find(|s| s.folder_name == "Base")
            .unwrap()
            .clone();
        let donor_cursor = scanned
            .iter()
            .find(|s| s.folder_name == "Donor")
            .unwrap()
            .elements
            .get("cursor.png")
            .cloned()
            .unwrap();

        let mut override_files = BTreeMap::new();
        override_files.insert("cursor.png", donor_cursor.clone());
        let outcome = save_skin(
            &base_skin,
            &[(
                "cursor".to_owned(),
                PoolEntry {
                    skin: "Donor".to_owned(),
                    files: override_files,
                    thumb: donor_cursor.clone(),
                    also_in: Vec::new(),
                    is_blank: false,
                },
            )],
            &[],
        )
        .unwrap();

        assert_eq!(outcome.name, "Base v1");
        let target = skins.join("Base v1");
        assert!(target.join("hitcircle@2x.png").exists(), "base art is kept");
        let ini = fs::read_to_string(target.join("skin.ini")).unwrap();
        assert!(ini.contains("Name: Base v1"));
        assert!(ini.contains("Author: Me"));
        // Pooled pick overwrote the base cursor under the canonical name.
        let replaced = image::io::Reader::open(target.join("cursor.png"))
            .unwrap()
            .decode()
            .unwrap();
        let pixel = replaced.to_rgba8().get_pixel(8, 8).0;
        assert_eq!(pixel, [0, 0, 255, 255]);
        assert_eq!(outcome.overrides_applied, 1);

        // Saving the derived skin again bumps along its own lineage.
        let derived = scan_skins(&skins)
            .into_iter()
            .find(|s| s.folder_name == "Base v1")
            .unwrap();
        assert_eq!(save_skin(&derived, &[], &[]).unwrap().name, "Base v2");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn decode_trims_transparent_padding() {
        let dir = unique_temp_dir("osu-skin-trim");
        let path = dir.join("padded.png");
        // 32x32 canvas, 16x16 opaque block in the middle — like a @2x-only
        // skin whose art sits at 1x scale inside a 2x canvas.
        let mut image = image::RgbaImage::from_pixel(32, 32, image::Rgba([0, 0, 0, 0]));
        for y in 8..24 {
            for x in 8..24 {
                image.put_pixel(x, y, image::Rgba([255, 0, 0, 255]));
            }
        }
        image.save(&path).unwrap();

        let decoded = decode_texture(&path, 128).unwrap();
        assert_eq!(
            (decoded.image.width(), decoded.image.height()),
            (16, 16),
            "transparent padding is cropped"
        );
        assert!(decoded.visible);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn decode_reports_transparency_and_1x_game_height() {
        let dir = unique_temp_dir("osu-skin-decode");
        let plain = dir.join("hitcircle.png");
        image::RgbaImage::from_pixel(128, 128, image::Rgba([255, 0, 0, 255]))
            .save(&plain)
            .unwrap();
        let decoded = decode_texture(&plain, 768).unwrap();
        assert!(decoded.visible);
        assert_eq!(decoded.game_height, 128.0);

        // @2x files count half their pixels in game units.
        let two_x = dir.join("hitcircle@2x.png");
        image::RgbaImage::from_pixel(256, 256, image::Rgba([255, 0, 0, 255]))
            .save(&two_x)
            .unwrap();
        let decoded = decode_texture(&two_x, 768).unwrap();
        assert_eq!(decoded.game_height, 128.0);

        // Instafade skins: a fully transparent layer decodes fine but is
        // flagged invisible — the game draws nothing for it.
        let invisible = dir.join("hitcircleoverlay@2x.png");
        image::RgbaImage::from_pixel(256, 256, image::Rgba([0, 0, 0, 0]))
            .save(&invisible)
            .unwrap();
        let decoded = decode_texture(&invisible, 768).unwrap();
        assert!(!decoded.visible);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn instafade_numbers_draw_at_game_scale() {
        const MOCK_CIRCLE: f32 = 118.0;
        let mut assets = SceneAssets {
            cursor: None,
            trail: None,
            middle: None,
            cursor_scale: 1.0,
            hitcircle: None,
            overlay: None,
            approach: None,
            hitcircle_game_height: None,
            overlay_game_height: None,
            approach_game_height: None,
            bar_bg: None,
            bar_colour: None,
            bar_marker: None,
            circle_digits: Default::default(),
            circle_digit_game_height: [None; 10],
            score_digits: Default::default(),
            score_x: None,
            score_comma: None,
            score_percent: None,
            combo_colours: Vec::new(),
            overlay_above_number: true,
            cursor_centre: true,
            cursor_expand: true,
            cursor_rotate: true,
            cursortrail_rotate: false,
        };
        assets.circle_digit_game_height[2] = Some(159.5); // 319 px of @2x art
        // Decoded digits always draw at game scale: 1x units, downscaled 0.8x
        // like the game, against the 128-unit reference circle. This covers
        // both normal digits and instafade skins (number carries circle art).
        let expected = 159.5 * DEFAULT_NUMBER_SCALE * MOCK_CIRCLE / GAME_CIRCLE_UNITS;
        assert!((assets.circle_digit_height(2, MOCK_CIRCLE) - expected).abs() < 1e-4);
        // Without a decoded digit there is nothing to scale — mock fraction,
        // also downscaled 0.8x.
        assert_eq!(
            assets.circle_digit_height(3, MOCK_CIRCLE),
            MOCK_CIRCLE * 0.4 * DEFAULT_NUMBER_SCALE
        );
    }

    #[test]
    fn circle_layers_keep_authored_relative_sizes() {
        // WhiteCat CK: a 102.5-unit fill inside a 118-unit ring (measured
        // @2x art, halved). Fitting both layers into one shared box would
        // stretch the fill over the ring and erase the visible inner edge.
        const MOCK_CIRCLE: f32 = 118.0;
        let fill = game_scaled_height(102.5, MOCK_CIRCLE);
        let ring = game_scaled_height(118.0, MOCK_CIRCLE);
        assert!(fill < ring);
        assert!((fill / ring - 102.5 / 118.0).abs() < 1e-4);
    }

    /// 64×64 canvas with a 32×32 opaque square centred (16..48), like a
    /// typical cursor: art in the middle, transparent padding around it.
    fn square_in_64_canvas(color: [u8; 4]) -> image::RgbaImage {
        let mut image = image::RgbaImage::from_pixel(64, 64, image::Rgba([0, 0, 0, 0]));
        for y in 16..48 {
            for x in 16..48 {
                image.put_pixel(x, y, image::Rgba(color));
            }
        }
        image
    }

    /// Bounds of the opaque region (`alpha > 8`, matching `trim_transparent`).
    fn opaque_bounds(image: &image::RgbaImage) -> (i32, i32, i32, i32) {
        let (width, height) = image.dimensions();
        let mut min_x = width as i32;
        let mut min_y = height as i32;
        let mut max_x = -1_i32;
        let mut max_y = -1_i32;
        for (x, y, pixel) in image.enumerate_pixels() {
            if pixel.0[3] > 8 {
                min_x = min_x.min(x as i32);
                max_x = max_x.max(x as i32);
                min_y = min_y.min(y as i32);
                max_y = max_y.max(y as i32);
            }
        }
        (min_x, min_y, max_x, max_y)
    }

    fn assert_close_to(value: i32, expected: i32) {
        assert!(
            (value - expected).abs() <= 1,
            "{value} should be within 1 px of {expected}"
        );
    }

    #[test]
    fn scale_within_canvas_keeps_resolution_and_anchors() {
        let art = square_in_64_canvas([255, 0, 0, 255]);

        // 50%, centre anchor (`CursorCentre` skins): the art shrinks to
        // ~16×16 in the middle while the canvas stays 64×64 — the game
        // keeps drawing the file at the same size, only the art inside it
        // gets smaller.
        let half = scale_within_canvas(&art, 0.5, true);
        assert_eq!(half.dimensions(), (64, 64));
        let (min_x, min_y, max_x, max_y) = opaque_bounds(&half);
        assert_close_to(min_x, 24);
        assert_close_to(min_y, 24);
        assert_close_to(max_x, 39);
        assert_close_to(max_y, 39);

        // Top-left anchor (`CursorCentre: 0`): the canvas corner stays
        // pinned, so the padded art moves toward the corner (16 → 8).
        let corner = scale_within_canvas(&art, 0.5, false);
        assert_eq!(corner.dimensions(), (64, 64));
        let (min_x, min_y, _, _) = opaque_bounds(&corner);
        assert_close_to(min_x, 8);
        assert_close_to(min_y, 8);

        // Upscaling clips at the canvas instead of growing the file: the
        // 32×32 art enlarged 2× now covers the whole 64×64 canvas.
        let big = scale_within_canvas(&art, 2.0, true);
        assert_eq!(big.dimensions(), (64, 64));
        let (min_x, min_y, max_x, max_y) = opaque_bounds(&big);
        assert_close_to(min_x, 0);
        assert_close_to(min_y, 0);
        assert_close_to(max_x, 63);
        assert_close_to(max_y, 63);

        // 100% is a pixel-identical round trip.
        let same = scale_within_canvas(&art, 1.0, true);
        assert_eq!(same.dimensions(), (64, 64));
        assert_eq!(opaque_bounds(&same), (16, 16, 47, 47));
    }

    #[test]
    fn save_resizes_base_files_without_changing_resolution() {
        let dir = unique_temp_dir("osu-skin-resize-base");
        let skins = dir.join("Skins");
        let base = skins.join("Base");
        fs::create_dir_all(&base).unwrap();
        fs::write(base.join("skin.ini"), "[General]\nName: Base\n").unwrap();
        square_in_64_canvas([255, 0, 0, 255])
            .save(base.join("cursor.png"))
            .unwrap();

        let base_skin = scan_skins(&skins)
            .into_iter()
            .find(|skin| skin.folder_name == "Base")
            .unwrap();
        let outcome = save_skin(&base_skin, &[], &[("cursor".to_owned(), 0.5)]).unwrap();
        assert_eq!(outcome.overrides_applied, 0);
        assert_eq!(outcome.resizes_applied, 1);

        let saved = image::io::Reader::open(skins.join("Base v1").join("cursor.png"))
            .unwrap()
            .decode()
            .unwrap()
            .to_rgba8();
        assert_eq!(saved.dimensions(), (64, 64), "resolution must not change");
        let (min_x, min_y, max_x, max_y) = opaque_bounds(&saved);
        assert_close_to(min_x, 24);
        assert_close_to(min_y, 24);
        assert_close_to(max_x, 39);
        assert_close_to(max_y, 39);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn save_resizes_pooled_overrides_without_changing_resolution() {
        let dir = unique_temp_dir("osu-skin-resize-override");
        let skins = dir.join("Skins");
        let base = skins.join("Base");
        let donor = skins.join("Donor");
        fs::create_dir_all(&base).unwrap();
        fs::create_dir_all(&donor).unwrap();
        fs::write(base.join("skin.ini"), "[General]\nName: Base\n").unwrap();
        square_in_64_canvas([255, 0, 0, 255])
            .save(base.join("cursor.png"))
            .unwrap();
        square_in_64_canvas([0, 0, 255, 255])
            .save(donor.join("cursor.png"))
            .unwrap();

        let scanned = scan_skins(&skins);
        let base_skin = scanned
            .iter()
            .find(|skin| skin.folder_name == "Base")
            .unwrap()
            .clone();
        let donor_cursor = scanned
            .iter()
            .find(|skin| skin.folder_name == "Donor")
            .unwrap()
            .elements
            .get("cursor.png")
            .cloned()
            .unwrap();
        let mut files = BTreeMap::new();
        files.insert("cursor.png", donor_cursor.clone());
        let outcome = save_skin(
            &base_skin,
            &[(
                "cursor".to_owned(),
                PoolEntry {
                    skin: "Donor".to_owned(),
                    files,
                    thumb: donor_cursor,
                    also_in: Vec::new(),
                    is_blank: false,
                },
            )],
            &[("cursor".to_owned(), 0.5)],
        )
        .unwrap();
        assert_eq!(outcome.overrides_applied, 1);
        assert_eq!(outcome.resizes_applied, 1);

        let saved = image::io::Reader::open(skins.join("Base v1").join("cursor.png"))
            .unwrap()
            .decode()
            .unwrap()
            .to_rgba8();
        assert_eq!(saved.dimensions(), (64, 64), "resolution must not change");
        // The donor's blue art, scaled down about the canvas centre.
        let centre = saved.get_pixel(32, 32).0;
        assert_eq!(centre, [0, 0, 255, 255]);
        let (min_x, min_y, max_x, _) = opaque_bounds(&saved);
        assert_close_to(min_x, 24);
        assert_close_to(min_y, 24);
        assert_close_to(max_x, 39);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn import_classifies_files_by_name() {
        let dir = unique_temp_dir("osu-skin-import-classify");
        write_png(&dir.join("cursor@2x.png"), [1, 2, 3, 255]);
        write_png(&dir.join("cursor.png"), [9, 9, 9, 255]);
        write_png(&dir.join("default-3.png"), [4, 5, 6, 255]);
        write_png(&dir.join("hitcircle.jpg"), [7, 8, 9, 255]);
        write_png(&dir.join("bogus-name.png"), [0, 0, 0, 255]);
        fs::write(dir.join("notes.txt"), b"x").unwrap();
        fs::write(dir.join("cursor.gif"), b"x").unwrap();

        let paths: Vec<PathBuf> = [
            "cursor@2x.png",
            "cursor.png",
            "default-3.png",
            "hitcircle.jpg",
            "bogus-name.png",
            "notes.txt",
            "cursor.gif",
        ]
        .iter()
        .map(|name| dir.join(name))
        .collect();
        let (placed, skipped) = classify_imports(&paths);

        // Both cursor variants land in the cursor slot, @2x winning.
        let cursor = &placed["cursor"];
        assert_eq!(cursor.len(), 1);
        assert_eq!(
            cursor["cursor.png"].file_name().and_then(|n| n.to_str()),
            Some("cursor@2x.png")
        );
        assert_eq!(
            placed["circle-numbers"]["default-3.png"],
            dir.join("default-3.png")
        );
        // The jpg keeps its container under the canonical key.
        assert_eq!(
            placed["hitcircle"]["hitcircle.png"]
                .extension()
                .and_then(|ext| ext.to_str()),
            Some("jpg")
        );
        for name in ["bogus-name.png", "notes.txt", "cursor.gif"] {
            assert!(skipped.iter().any(|s| s == name), "{name} must be skipped");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn import_files_pool_apply_and_prune() {
        let dir = unique_temp_dir("osu-skin-import-state");
        let cursor = dir.join("cursor.png");
        let trail = dir.join("cursortrail.png");
        write_png(&cursor, [1, 1, 1, 255]);
        write_png(&trail, [2, 2, 2, 255]);

        let mut state = SkinEditorState::new();
        state.import_files(vec![cursor.clone(), trail.clone()]);
        assert_eq!(state.imported["cursor"].len(), 1);
        assert_eq!(state.imported["cursortrail"].len(), 1);
        assert_eq!(state.overrides["cursor"].skin, IMPORTED_SKIN);
        assert_eq!(state.overrides["cursor"].files["cursor.png"], cursor);
        assert!(state.import_ok);
        let status = state.import_status.clone().unwrap();
        assert!(status.contains("2 file(s)"), "{status}");

        // Re-importing identical files collapses into the existing tile and
        // keeps the pick.
        state.import_files(vec![cursor.clone()]);
        assert_eq!(state.imported["cursor"].len(), 1);
        assert_eq!(state.overrides["cursor"].files["cursor.png"], cursor);

        // A vanished source file drops the import; the pick built on it goes
        // with the same prune the scan result runs.
        fs::remove_file(&cursor).unwrap();
        state.retain_live_imports();
        assert!(!state.imported.contains_key("cursor"));
        assert!(state.imported.contains_key("cursortrail"));
        state.overrides.retain(|_, entry| entry.path_exists());
        assert!(!state.overrides.contains_key("cursor"));
        assert!(state.overrides.contains_key("cursortrail"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn import_folder_walks_subfolders() {
        let dir = unique_temp_dir("osu-skin-import-folder");
        let nested = dir.join("Assets").join("default");
        fs::create_dir_all(&nested).unwrap();
        write_png(&dir.join("cursor.png"), [1, 1, 1, 255]);
        write_png(&nested.join("default-2@2x.png"), [2, 2, 2, 255]);
        fs::write(dir.join("skin.ini"), b"[General]\nName: X\n").unwrap();

        let mut state = SkinEditorState::new();
        state.import_folder(&dir);
        assert!(state.import_ok, "{}", state.import_status.clone().unwrap());
        assert_eq!(state.imported["cursor"].len(), 1);
        assert_eq!(state.imported["circle-numbers"].len(), 1);
        assert_eq!(
            state.overrides["circle-numbers"].files["default-2.png"],
            nested.join("default-2@2x.png")
        );
        // Non-image files (skin.ini) are filtered before classification, so
        // they are not reported as skipped surprises.
        let status = state.import_status.clone().unwrap();
        assert!(!status.contains("skin.ini"), "{status}");
        let _ = fs::remove_dir_all(dir);
    }
}
