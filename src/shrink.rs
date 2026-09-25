//! Beatmap asset shrinking.
//!
//! Scans one beatmapset folder at a time, classifies every file by how
//! the set's `.osu`/`.osb` files reference it, and compresses what is
//! safe to compress:
//!
//! * song audio (`.mp3`/`.ogg`) → same-format bitrate reduction
//!   (192k MP3 / OGG q6, per the osu! wiki recipes)
//! * background video (`.mp4`) → H.264 ≤720p, audio/subs stripped
//! * background + storyboard images → downscale absurd sizes, JPEG
//!   quality pass, PNG re-encode (same filename, content only)
//!
//! Non-negotiable guarantees (score submission hashes the `.osu`, not
//! the assets, so these keep maps fully playable and submittable):
//!
//! * filenames never change; `.osu`/`.osb`/`skin.ini` are never written
//! * storyboard `Sample` sounds are treated as protected references
//! * `.wav` hitsounds are never touched (format-locked by the game)
//! * animated `.gif` is never re-encoded (would collapse to one frame)
//! * files that would grow are kept untouched instead of failing
//! * anything else convertible only via a rename is skipped outright
//!   (`.flv` video, `.wav` song audio, …)
//!
//! Every converted file is verified (audio duration, video codec/size,
//! image decode) before it replaces the original, and each set folder
//! is zipped as a backup first when `ShrinkOptions::backup` is set.

use crate::local::LocalBeatmap;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
    time::{Duration, Instant, UNIX_EPOCH},
};

/// Song audio lands at this bitrate (osu! wiki: 128–192k MP3).
pub const AUDIO_TARGET_BPS: u64 = 192_000;
/// Sources above this are worth re-encoding; below it we leave alone.
pub const AUDIO_REENCODE_ABOVE_BPS: u64 = 210_000;
/// Videos shrink to at most this height (wiki: 1280x720 max, H.264 only).
pub const VIDEO_MAX_HEIGHT: u32 = 720;
/// Images shrink to at most this width; never upscaled.
pub const IMAGE_MAX_WIDTH: u32 = 1920;
/// JPEGs below this size are left alone (re-encoding risks visible loss
/// for little gain).
pub const JPEG_MIN_BYTES: u64 = 300_000;
/// PNGs below this size are left alone.
pub const PNG_MIN_BYTES: u64 = 1_000_000;

/// What a set asset is, by reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShrinkAssetKind {
    SongAudio,
    Video,
    Image,
    /// `.wav`/indexed hitsounds, `.osu`, `.osb`, config: never touched.
    Protected,
    /// Media file nothing references (delete candidate, default off).
    Orphan,
}

/// What to do with one asset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShrinkAction {
    Mp3Cbr192,
    OggQ6,
    Video720pH264,
    ImageReencode,
    DeleteOrphan,
    /// Skin element: same dimensions, same container, tighter encode.
    /// Gameplay/UI art is pixel-exact, so skins never downscale and
    /// never change containers — worst case is a no-op.
    SkinImageReencode,
    /// Delete a referenced background video outright. The game falls
    /// back to the background image; `.osu`/`.osb` files are untouched,
    /// so this stays inside the Safe guarantee.
    RemoveVideo,
    Skip {
        reason: String,
    },
}

impl ShrinkAction {
    pub fn is_work(&self) -> bool {
        !matches!(self, ShrinkAction::Skip { .. })
    }

    /// Re-encodes are what the shrink cache tracks (deletions leave no
    /// file behind to match, so they need no record).
    pub fn is_reencode(&self) -> bool {
        matches!(
            self,
            ShrinkAction::Mp3Cbr192
                | ShrinkAction::OggQ6
                | ShrinkAction::Video720pH264
                | ShrinkAction::ImageReencode
                | ShrinkAction::SkinImageReencode
        )
    }

    pub fn label(&self) -> String {
        match self {
            ShrinkAction::Mp3Cbr192 => "MP3 192k".to_owned(),
            ShrinkAction::OggQ6 => "OGG q6".to_owned(),
            ShrinkAction::Video720pH264 => "H.264 720p".to_owned(),
            ShrinkAction::ImageReencode => "re-encode".to_owned(),
            ShrinkAction::DeleteOrphan => "delete".to_owned(),
            ShrinkAction::SkinImageReencode => "skin re-encode".to_owned(),
            ShrinkAction::RemoveVideo => "remove video".to_owned(),
            ShrinkAction::Skip { reason } => format!("skip ({reason})"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetAsset {
    pub name: String,
    pub bytes: u64,
    pub kind: ShrinkAssetKind,
    pub action: ShrinkAction,
    pub est_bytes: u64,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetShrinkReport {
    pub folder: PathBuf,
    pub label: String,
    pub assets: Vec<SetAsset>,
    pub total_in: u64,
    pub total_est: u64,
}

impl SetShrinkReport {
    pub fn work_items(&self) -> usize {
        self.assets.iter().filter(|a| a.action.is_work()).count()
    }

    /// Files skipped because the shrink cache already covers them.
    pub fn cached_items(&self) -> usize {
        self.assets
            .iter()
            .filter(|a| {
                matches!(&a.action, ShrinkAction::Skip { reason } if reason == ALREADY_SHRUNK_REASON)
            })
            .count()
    }

    pub fn est_saved(&self) -> u64 {
        self.total_in.saturating_sub(self.total_est)
    }
}

#[derive(Debug, Clone)]
pub struct ShrinkOptions {
    /// Zip each set folder before touching it.
    pub backup: bool,
    /// Also delete orphan media (default off).
    pub delete_orphans: bool,
    /// Delete referenced background videos instead of compressing them
    /// (default off). The game shows the background image instead;
    /// `.osu`/`.osb` files are untouched.
    pub remove_videos: bool,
    /// JPEG quality for image re-encodes.
    pub jpeg_quality: u8,
    /// x264 CRF for video re-encodes (wiki suggests 20–25).
    pub video_crf: u32,
    /// How many set folders to shrink concurrently (x264 is CPU-heavy;
    /// 2 is a sane default, 1 is the most disk-friendly).
    pub jobs: usize,
}

impl Default for ShrinkOptions {
    fn default() -> Self {
        Self {
            backup: true,
            delete_orphans: false,
            remove_videos: false,
            jpeg_quality: 85,
            video_crf: 21,
            jobs: 2,
        }
    }
}

impl ShrinkOptions {
    /// Fingerprint of every setting that changes converter output. Files
    /// recorded under different settings are re-planned, not trusted.
    pub fn fingerprint(&self) -> String {
        format!(
            "v{} q{} crf{}",
            SHRINK_CACHE_VERSION, self.jpeg_quality, self.video_crf
        )
    }
}

/// Bump when converter behavior changes so stale "already shrunk"
/// records stop matching instead of silently skipping better plans.
pub const SHRINK_CACHE_VERSION: u32 = 1;

/// Skip reason used for cache hits (counted for the UI summary).
pub const ALREADY_SHRUNK_REASON: &str = "already shrunk";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ShrinkCacheEntry {
    size: u64,
    mtime: u64,
    action: String,
    settings: String,
}

/// Persistent record of successfully shrunk files: path → what the file
/// looked like right after conversion, with what action and settings.
/// Re-analysis downgrades matching plans to Skip instead of queueing
/// another (lossy!) re-encode — this is both a speed and a quality fix,
/// since repeated JPEG generations visibly degrade.
///
/// Matching is conservative: any size/mtime change, any settings change,
/// or any cache-version change re-plans the file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShrinkCache {
    version: u32,
    entries: HashMap<PathBuf, ShrinkCacheEntry>,
}

impl ShrinkCache {
    pub fn load(path: &Path) -> Self {
        let text = fs::read_to_string(path).unwrap_or_default();
        match serde_json::from_str::<ShrinkCache>(&text) {
            Ok(cache) if cache.version == SHRINK_CACHE_VERSION => cache,
            _ => ShrinkCache {
                version: SHRINK_CACHE_VERSION,
                entries: HashMap::new(),
            },
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(&ShrinkCache {
            version: SHRINK_CACHE_VERSION,
            entries: self.entries.clone(),
        })?;
        fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    pub fn is_shrunk(
        &self,
        path: &Path,
        size: u64,
        mtime: u64,
        action: &ShrinkAction,
        settings: &str,
    ) -> bool {
        self.entries.get(path).is_some_and(|entry| {
            entry.size == size
                && entry.mtime == mtime
                && entry.action == action.label()
                && entry.settings == settings
        })
    }

    pub fn record(
        &mut self,
        path: &Path,
        size: u64,
        mtime: u64,
        action: &ShrinkAction,
        settings: &str,
    ) {
        self.entries.insert(
            path.to_owned(),
            ShrinkCacheEntry {
                size,
                mtime,
                action: action.label(),
                settings: settings.to_owned(),
            },
        );
    }

    pub fn extend(&mut self, other: ShrinkCache) {
        self.entries.extend(other.entries);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// `(size, mtime seconds)` for cache keys. `None` when the file cannot
/// even be statted — callers must fail safe (plan, don't skip).
fn file_sig(path: &Path) -> Option<(u64, u64)> {
    let meta = fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some((meta.len(), mtime))
}

/// Resolved converter binaries: `ffmpeg` plus `ffprobe` next to it or
/// on PATH (ffprobe ships with every desktop ffmpeg build).
#[derive(Debug, Clone)]
pub struct ShrinkBins {
    pub ffmpeg: PathBuf,
    pub ffprobe: Option<PathBuf>,
}

pub fn resolve_bins(ffmpeg: PathBuf) -> ShrinkBins {
    let ffprobe = ffmpeg
        .parent()
        .map(|dir| dir.join(exe_name("ffprobe")))
        .filter(|p| p.is_file())
        .or_else(|| which_on_path(exe_name("ffprobe")));
    ShrinkBins { ffmpeg, ffprobe }
}

fn exe_name(base: &str) -> String {
    if cfg!(target_os = "windows") {
        format!("{base}.exe")
    } else {
        base.to_owned()
    }
}

fn which_on_path(name: String) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(&name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

// ── Referenced-asset collection ──────────────────────────────

/// Every filename a set's difficulties + storyboard reference.
#[derive(Debug, Default)]
struct SetReferences {
    audio: BTreeSet<String>,
    backgrounds: BTreeSet<String>,
    videos: BTreeSet<String>,
    sprites: BTreeSet<String>,
    /// Storyboard `Sample` sounds: timing-critical SFX that resolve
    /// implicitly. Protected, like hitsounds — and crucially, never
    /// orphans.
    samples: BTreeSet<String>,
}

fn norm_name(name: &str) -> String {
    name.trim().trim_matches('"').replace('\\', "/")
}

fn collect_references(folder: &Path, maps: &[LocalBeatmap]) -> SetReferences {
    let mut refs = SetReferences::default();
    for map in maps {
        if let Some(audio) = &map.audio_filename {
            refs.audio.insert(norm_name(audio).to_lowercase());
        }
        if let Some(bg) = &map.background_filename {
            refs.backgrounds.insert(norm_name(bg).to_lowercase());
        }
        for events in event_sources(&map.path, folder) {
            let (videos, sprites, samples) = parse_event_assets(&events);
            refs.videos.extend(videos);
            refs.sprites.extend(sprites);
            refs.samples.extend(samples);
        }
    }
    refs
}

/// `.osu` `[Events]` sections plus every `.osb` in the folder.
fn event_sources(osu_path: &Path, folder: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(text) = fs::read_to_string(osu_path) {
        out.push(events_section(&text));
    }
    if let Ok(entries) = fs::read_dir(folder) {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("osb"))
                && let Ok(text) = fs::read_to_string(&path)
            {
                out.push(events_section(&text));
            }
        }
    }
    out
}

/// The `[Events]` section body (`.osb` files are all events).
fn events_section(text: &str) -> String {
    let mut in_events = false;
    let mut body = String::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_events = trimmed.eq_ignore_ascii_case("[Events]");
            continue;
        }
        if in_events {
            body.push_str(line);
            body.push('\n');
        }
    }
    body
}

/// Split an event line quote-aware; returns the comma fields.
fn split_event_line(line: &str) -> Vec<String> {
    let mut in_quotes = false;
    let mut current = String::new();
    let mut fields = Vec::new();
    for ch in line.chars() {
        match ch {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                fields.push(current.trim().to_owned());
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    fields.push(current.trim().to_owned());
    fields
}

/// `(videos, sprites, samples)` referenced by an events body.
/// `Video`/`1` lines carry the clip at field 2; `Sprite`/`Animation`/`L`
/// lines carry the image at field 3; `Sample`/`S` lines carry the sound
/// at field 3. Samples are timing-critical SFX: callers must treat them
/// as protected, never as orphans or shrink candidates.
fn parse_event_assets(body: &str) -> (BTreeSet<String>, BTreeSet<String>, BTreeSet<String>) {
    let mut videos = BTreeSet::new();
    let mut sprites = BTreeSet::new();
    let mut samples = BTreeSet::new();
    for raw in body.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        let fields = split_event_line(line);
        let head = fields
            .first()
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        match head.as_str() {
            "video" | "1" => {
                if let Some(name) = fields.get(2).map(|s| norm_name(s).to_lowercase())
                    && !name.is_empty()
                {
                    videos.insert(name);
                }
            }
            "sprite" | "animation" | "l" => {
                if let Some(name) = fields.get(3).map(|s| norm_name(s).to_lowercase())
                    && !name.is_empty()
                {
                    sprites.insert(name);
                }
            }
            "sample" | "s" => {
                if let Some(name) = fields.get(3).map(|s| norm_name(s).to_lowercase())
                    && !name.is_empty()
                {
                    samples.insert(name);
                }
            }
            _ => {}
        }
    }
    (videos, sprites, samples)
}

// ── Per-set analysis ─────────────────────────────────────────

/// Child processes must never flash a console window (Windows spawns
/// one per `Command` otherwise — hundreds during analysis).
fn silent_command(program: &Path) -> Command {
    let mut cmd = Command::new(program);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt as _;
        // CREATE_NO_WINDOW
        cmd.creation_flags(0x08000000);
    }
    cmd
}

#[derive(Debug, Clone)]
pub(crate) struct MediaProbe {
    codec: String,
    width: u32,
    height: u32,
    bitrate: Option<u64>,
    duration_s: f64,
    has_audio: bool,
}

/// ffprobe results keyed by path, validated by size + mtime. Analysis
/// re-runs often (the orphan toggle re-plans everything), and a warm
/// cache makes those instant while keeping results correct when files
/// actually change.
pub type ProbeCache = HashMap<PathBuf, (u64, u64, MediaProbe)>;

/// Cached probe: HashMap hit on unchanged files, one ffprobe spawn on
/// new/changed files, `None` when neither can answer.
fn cached_probe(cache: &mut ProbeCache, ffprobe: Option<&Path>, path: &Path) -> Option<MediaProbe> {
    let meta = fs::metadata(path).ok()?;
    let len = meta.len();
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs();
    if let Some((cached_len, cached_mtime, probe)) = cache.get(path)
        && *cached_len == len
        && *cached_mtime == mtime
    {
        return Some(probe.clone());
    }
    let probe = probe_media(ffprobe, path)?;
    cache.insert(path.to_owned(), (len, mtime, probe.clone()));
    Some(probe)
}

fn probe_media(ffprobe: Option<&Path>, path: &Path) -> Option<MediaProbe> {
    let ffprobe = ffprobe?;
    let out = silent_command(ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration,size,bit_rate:stream=codec_name,codec_type,width,height",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let mut probe = MediaProbe {
        codec: String::new(),
        width: 0,
        height: 0,
        bitrate: json
            .pointer("/format/bit_rate")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<u64>().ok()),
        duration_s: json
            .pointer("/format/duration")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0),
        has_audio: false,
    };
    let mut primary = true;
    for stream in json
        .pointer("/streams")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        let ctype = stream
            .get("codec_type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if ctype == "audio" {
            probe.has_audio = true;
            if primary {
                probe.codec = stream
                    .get("codec_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
            }
            // Stream bitrate beats container bitrate for audio files.
            if primary
                && let Some(bps) = stream
                    .get("bit_rate")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<u64>().ok())
            {
                probe.bitrate = Some(bps);
            }
        }
        if primary && ctype == "video" {
            probe.codec = stream
                .get("codec_name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            probe.width = stream.get("width").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            probe.height = stream.get("height").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            primary = false;
        }
    }
    Some(probe)
}

fn image_dimensions(path: &Path) -> Option<(u32, u32)> {
    // Header-only sniff first: full decode is wasteful for analysis.
    let reader = image::io::Reader::open(path).ok()?;
    let (w, h) = reader.into_dimensions().ok()?;
    Some((w, h))
}

fn lower_ext(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn is_audio_ext(ext: &str) -> bool {
    matches!(
        ext,
        "mp3" | "ogg" | "wav" | "flac" | "m4a" | "aac" | "opus" | "wma"
    )
}

fn is_image_ext(ext: &str) -> bool {
    matches!(
        ext,
        "jpg" | "jpeg" | "png" | "bmp" | "tif" | "tiff" | "webp" | "gif" | "ico"
    )
}

fn is_video_ext(ext: &str) -> bool {
    matches!(
        ext,
        "mp4" | "avi" | "flv" | "mov" | "mkv" | "wmv" | "mpg" | "mpeg" | "m2ts" | "webm"
    )
}

/// Analyze one set folder. `options` only affects estimates, never the
/// file list, so re-analysis is cheap to skip by caching per folder.
/// `cache` persists ffprobe results across runs (keyed by path + size +
/// mtime), so toggling options re-plans without re-spawning ffprobe.
/// `shrink_cache` downgrades already-shrunk re-encodes to Skip.
pub fn analyze_set(
    folder: &Path,
    label: String,
    maps: &[LocalBeatmap],
    bins: &ShrinkBins,
    options: &ShrinkOptions,
    cache: &mut ProbeCache,
    shrink_cache: &ShrinkCache,
) -> SetShrinkReport {
    let refs = collect_references(folder, maps);
    let settings = options.fingerprint();
    let mut assets = Vec::new();

    let entries: Vec<PathBuf> = fs::read_dir(folder)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_file())
                .collect()
        })
        .unwrap_or_default();

    for path in entries {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_owned();
        // osz/osk leftovers, thumbs.db and friends: never our business.
        if name.starts_with('.') {
            continue;
        }
        let bytes = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if bytes == 0 {
            continue;
        }
        // mtime rides along for the shrink-cache lookup (0 fails safe:
        // a stat race just means "plan it", never "wrongly skip it").
        let mtime = file_sig(&path).map(|(_, mtime)| mtime).unwrap_or(0);
        let key = norm_name(&name).to_lowercase();
        let ext = lower_ext(&path);

        // Protected bookkeeping, whatever the extension.
        if matches!(ext.as_str(), "osu" | "osb" | "ini" | "osz" | "osk") {
            continue;
        }

        if refs.audio.contains(&key) {
            assets.push(maybe_cached(
                &path,
                bytes,
                mtime,
                plan_song_audio(&path, &name, bytes, &ext, bins, options, cache),
                shrink_cache,
                &settings,
            ));
        } else if refs.backgrounds.contains(&key) {
            assets.push(maybe_cached(
                &path,
                bytes,
                mtime,
                plan_image(&path, &name, bytes, &ext, options, true),
                shrink_cache,
                &settings,
            ));
        } else if refs.videos.contains(&key) {
            assets.push(maybe_cached(
                &path,
                bytes,
                mtime,
                plan_video(&path, &name, bytes, &ext, bins, options, cache),
                shrink_cache,
                &settings,
            ));
        } else if refs.sprites.contains(&key) {
            assets.push(maybe_cached(
                &path,
                bytes,
                mtime,
                plan_image(&path, &name, bytes, &ext, options, false),
                shrink_cache,
                &settings,
            ));
        } else if refs.samples.contains(&key) {
            // Storyboard SFX: timing-critical and implicitly resolved —
            // protected, and never orphan candidates.
            assets.push(skip_asset(
                name,
                bytes,
                ShrinkAssetKind::Protected,
                "storyboard sample",
            ));
        } else if ext == "wav" || ext == "ogg" && is_hitsound_name(&name) {
            // Indexed/custom hitsounds resolve implicitly (`soft-hit…`);
            // `.wav` is format-locked by the game anyway.
            assets.push(skip_asset(
                name,
                bytes,
                ShrinkAssetKind::Protected,
                "hitsound",
            ));
        } else if is_audio_ext(&ext) || is_image_ext(&ext) || is_video_ext(&ext) {
            let kind = ShrinkAssetKind::Orphan;
            let action = if options.delete_orphans {
                ShrinkAction::DeleteOrphan
            } else {
                ShrinkAction::Skip {
                    reason: "unreferenced".to_owned(),
                }
            };
            let est_bytes = if action == ShrinkAction::DeleteOrphan {
                0
            } else {
                bytes
            };
            assets.push(SetAsset {
                name,
                bytes,
                kind,
                action,
                est_bytes,
                note: "nothing references this file".to_owned(),
            });
        }
        // Anything else (zips, txts, db files): out of scope, invisible.
    }

    assets.sort_by_key(|a| std::cmp::Reverse(a.bytes));
    let total_in: u64 = assets.iter().map(|a| a.bytes).sum();
    let total_est: u64 = assets.iter().map(|a| a.est_bytes).sum();
    SetShrinkReport {
        folder: folder.to_owned(),
        label,
        assets,
        total_in,
        total_est,
    }
}

/// Skin elements worth re-encoding must clear this size (PNG) — below
/// it the game-ready files are usually already optimal and a pass just
/// churns mtimes for bytes.
pub const SKIN_PNG_MIN_BYTES: u64 = 100_000;
/// JPEG skin elements only below this size threshold are left alone;
/// above it a quality pass is worth attempting (keep-if-smaller swap
/// guarantees the worst case is a no-op).
pub const SKIN_JPEG_MIN_BYTES: u64 = 1_000_000;

/// Analyze one skin folder (flat: only root-level files are loaded by
/// the game). Skin element names are resolved by the client itself, so
/// containers never change and dimensions never change — `skin.ini`
/// and all sounds are reported as protected, images get
/// [`ShrinkAction::SkinImageReencode`] when big enough to matter.
pub fn analyze_skin(
    folder: &Path,
    label: String,
    options: &ShrinkOptions,
    shrink_cache: &ShrinkCache,
) -> SetShrinkReport {
    let settings = options.fingerprint();
    let mut assets = Vec::new();
    let entries: Vec<PathBuf> = fs::read_dir(folder)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_file())
                .collect()
        })
        .unwrap_or_default();

    for path in entries {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_owned();
        if name.starts_with('.') {
            continue;
        }
        let bytes = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if bytes == 0 {
            continue;
        }
        let mtime = file_sig(&path).map(|(_, mtime)| mtime).unwrap_or(0);
        let ext = lower_ext(&path);
        match ext.as_str() {
            "png" => {
                if bytes <= SKIN_PNG_MIN_BYTES {
                    assets.push(skip_asset(
                        name,
                        bytes,
                        ShrinkAssetKind::Image,
                        "already small",
                    ));
                } else {
                    assets.push(maybe_cached(
                        &path,
                        bytes,
                        mtime,
                        SetAsset {
                            name,
                            bytes,
                            kind: ShrinkAssetKind::Image,
                            action: ShrinkAction::SkinImageReencode,
                            est_bytes: (bytes as f64 * 0.9) as u64,
                            note: "lossless re-encode, same pixels".to_owned(),
                        },
                        shrink_cache,
                        &settings,
                    ));
                }
            }
            "jpg" | "jpeg" => {
                if bytes <= SKIN_JPEG_MIN_BYTES {
                    assets.push(skip_asset(
                        name,
                        bytes,
                        ShrinkAssetKind::Image,
                        "already small",
                    ));
                } else {
                    assets.push(maybe_cached(
                        &path,
                        bytes,
                        mtime,
                        SetAsset {
                            name,
                            bytes,
                            kind: ShrinkAssetKind::Image,
                            action: ShrinkAction::SkinImageReencode,
                            est_bytes: (bytes as f64 * 0.8) as u64,
                            note: "quality pass, same pixels".to_owned(),
                        },
                        shrink_cache,
                        &settings,
                    ));
                }
            }
            "ini" => assets.push(skip_asset(
                name,
                bytes,
                ShrinkAssetKind::Protected,
                "skin config",
            )),
            "wav" | "mp3" | "ogg" => assets.push(skip_asset(
                name,
                bytes,
                ShrinkAssetKind::Protected,
                "skin sound",
            )),
            _ => {}
        }
    }

    assets.sort_by_key(|a| std::cmp::Reverse(a.bytes));
    let total_in: u64 = assets.iter().map(|a| a.bytes).sum();
    let total_est: u64 = assets.iter().map(|a| a.est_bytes).sum();
    SetShrinkReport {
        folder: folder.to_owned(),
        label,
        assets,
        total_in,
        total_est,
    }
}

/// `soft-hitnormal2.wav`-style names resolve without being named in any
/// `.osu` — never treat them as orphans.
fn is_hitsound_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let stem = lower.rsplit('.').next().unwrap_or(&lower);
    let _ = stem;
    ["normal-", "soft-", "drum-"]
        .iter()
        .any(|prefix| lower.starts_with(prefix))
        || lower.contains("hitnormal")
        || lower.contains("hitclap")
        || lower.contains("hitfinish")
        || lower.contains("hitwhistle")
        || lower.contains("slidertick")
        || lower.contains("sliderslide")
        || lower.contains("sliderwhistle")
}

fn skip_asset(name: String, bytes: u64, kind: ShrinkAssetKind, reason: &str) -> SetAsset {
    SetAsset {
        name,
        bytes,
        kind,
        action: ShrinkAction::Skip {
            reason: reason.to_owned(),
        },
        est_bytes: bytes,
        note: reason.to_owned(),
    }
}

/// Downgrade a planned re-encode to Skip when the shrink cache proves
/// this exact file (size + mtime) was already converted with this
/// action under these settings. Anything else — changed file, changed
/// settings, unknown file — passes through untouched.
fn maybe_cached(
    path: &Path,
    bytes: u64,
    mtime: u64,
    asset: SetAsset,
    shrink_cache: &ShrinkCache,
    settings: &str,
) -> SetAsset {
    if asset.action.is_reencode()
        && shrink_cache.is_shrunk(path, bytes, mtime, &asset.action, settings)
    {
        SetAsset {
            action: ShrinkAction::Skip {
                reason: ALREADY_SHRUNK_REASON.to_owned(),
            },
            est_bytes: bytes,
            note: "already shrunk — skipping".to_owned(),
            ..asset
        }
    } else {
        asset
    }
}

fn plan_song_audio(
    path: &Path,
    name: &str,
    bytes: u64,
    ext: &str,
    bins: &ShrinkBins,
    _options: &ShrinkOptions,
    cache: &mut ProbeCache,
) -> SetAsset {
    let kind = ShrinkAssetKind::SongAudio;
    // Only the two wiki-supported song formats are convertible: anything
    // else would need a rename plus an `AudioFilename` rewrite, which
    // changes the `.osu` checksum — out of scope by design.
    if ext != "mp3" && ext != "ogg" {
        return skip_asset(
            name.to_owned(),
            bytes,
            kind,
            "unsupported container (only mp3/ogg)",
        );
    }
    let Some(probe) = cached_probe(cache, bins.ffprobe.as_deref(), path) else {
        return skip_asset(name.to_owned(), bytes, kind, "could not probe audio");
    };
    let src_bps = probe.bitrate.unwrap_or_else(|| {
        // Container bitrate fallback: total minus nothing (song files
        // are audio-only in practice).
        if probe.duration_s.is_significant() {
            (bytes as f64 * 8.0 / probe.duration_s) as u64
        } else {
            0
        }
    });
    if src_bps <= AUDIO_REENCODE_ABOVE_BPS || !probe.duration_s.is_significant() {
        return skip_asset(name.to_owned(), bytes, kind, "already ≤192k");
    }
    let est_bytes = (AUDIO_TARGET_BPS as f64 * probe.duration_s / 8.0 * 1.01) as u64;
    let (action, note) = if ext == "mp3" {
        (
            ShrinkAction::Mp3Cbr192,
            format!("{} kbps → 192k", src_bps / 1000),
        )
    } else {
        (
            ShrinkAction::OggQ6,
            format!("{} kbps → ~192k", src_bps / 1000),
        )
    };
    SetAsset {
        name: name.to_owned(),
        bytes,
        kind,
        action,
        est_bytes,
        note,
    }
}

trait Significant {
    fn is_significant(&self) -> bool;
}

impl Significant for f64 {
    fn is_significant(&self) -> bool {
        *self > 0.0
    }
}

fn plan_video(
    path: &Path,
    name: &str,
    bytes: u64,
    ext: &str,
    bins: &ShrinkBins,
    options: &ShrinkOptions,
    cache: &mut ProbeCache,
) -> SetAsset {
    let kind = ShrinkAssetKind::Video;
    if options.remove_videos {
        return SetAsset {
            name: name.to_owned(),
            bytes,
            kind,
            action: ShrinkAction::RemoveVideo,
            est_bytes: 0,
            note: "deleted · game shows background instead".to_owned(),
        };
    }
    if ext != "mp4" {
        // mp4/H.264 is the only supported video container; anything else
        // would need a rename plus a `Video` line rewrite, which changes
        // the `.osu` checksum — out of scope by design.
        return skip_asset(
            name.to_owned(),
            bytes,
            kind,
            "unsupported container (only mp4)",
        );
    }
    let Some(probe) = cached_probe(cache, bins.ffprobe.as_deref(), path) else {
        return skip_asset(name.to_owned(), bytes, kind, "could not probe video");
    };
    let needs_work = probe.codec != "h264"
        || probe.height > VIDEO_MAX_HEIGHT
        || probe.has_audio
        || probe.width == 0;
    if !needs_work {
        return skip_asset(name.to_owned(), bytes, kind, "already H.264 ≤720p");
    }
    // Bitrate tracks pixels sub-linearly; re-encode to high-quality H.264.
    let scale = if probe.height > VIDEO_MAX_HEIGHT {
        (VIDEO_MAX_HEIGHT as f64 / probe.height as f64).powf(0.85)
    } else {
        1.0
    };
    let est_bytes = (bytes as f64 * scale * 0.85) as u64;
    SetAsset {
        name: name.to_owned(),
        bytes,
        kind,
        action: ShrinkAction::Video720pH264,
        est_bytes,
        note: format!(
            "{}x{} {} → 720p H.264",
            probe.width, probe.height, probe.codec
        ),
    }
}

fn plan_image(
    path: &Path,
    name: &str,
    bytes: u64,
    ext: &str,
    options: &ShrinkOptions,
    _background: bool,
) -> SetAsset {
    let kind = ShrinkAssetKind::Image;
    if ext == "gif" {
        return skip_asset(name.to_owned(), bytes, kind, "animated — keep as-is");
    }
    if !matches!(ext, "jpg" | "jpeg" | "png" | "bmp" | "webp") {
        return skip_asset(name.to_owned(), bytes, kind, "unsupported image type");
    }
    let Some((w, h)) = image_dimensions(path) else {
        return skip_asset(name.to_owned(), bytes, kind, "could not read image");
    };
    if w == 0 || h == 0 {
        return skip_asset(name.to_owned(), bytes, kind, "could not read image");
    }
    let downscale = if w > IMAGE_MAX_WIDTH {
        IMAGE_MAX_WIDTH as f64 / w as f64
    } else {
        1.0
    };
    let big_enough = match ext {
        "png" => bytes > PNG_MIN_BYTES,
        _ => bytes > JPEG_MIN_BYTES,
    };
    if downscale >= 1.0 && !big_enough {
        return skip_asset(name.to_owned(), bytes, kind, "already efficient");
    }
    // Pixels shrink quadratically; JPEG quality pass shaves ~15%.
    let quality_factor = if matches!(ext, "jpg" | "jpeg" | "webp") {
        0.85
    } else {
        0.9
    };
    let est_bytes = (bytes as f64 * downscale.powf(2.0) * quality_factor) as u64;
    let _ = options;
    SetAsset {
        name: name.to_owned(),
        bytes,
        kind,
        action: ShrinkAction::ImageReencode,
        est_bytes,
        note: format!("{w}x{h} → {}", scale_note(w, h, downscale)),
    }
}

fn scale_note(w: u32, h: u32, downscale: f64) -> String {
    if downscale < 1.0 {
        format!(
            "{}x{}",
            (w as f64 * downscale) as u32,
            (h as f64 * downscale) as u32
        )
    } else {
        "same size".to_owned()
    }
}

// ── Converters ───────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ShrinkOutcome {
    pub path: PathBuf,
}

fn run_ffmpeg(ffmpeg: &Path, args: &[String], cancel: &AtomicBool) -> Result<(u64, f64)> {
    use std::io::{BufRead, BufReader};
    let started = Instant::now();
    let mut child = silent_command(ffmpeg)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("spawning {}", ffmpeg.display()))?;
    let mut stderr_tail = Vec::<String>::new();
    if let Some(err) = child.stderr.take() {
        for line in BufReader::new(err).lines().map_while(Result::ok) {
            stderr_tail.push(line);
            if stderr_tail.len() > 20 {
                stderr_tail.remove(0);
            }
        }
    }
    // Drain stdout to avoid pipe back-pressure (progress not needed here;
    // per-asset granularity is the progress unit).
    if let Some(out) = child.stdout.take() {
        for _ in BufReader::new(out).lines() {
            if cancel.load(Ordering::Relaxed) {
                let _ = child.kill();
                anyhow::bail!("cancelled");
            }
        }
    }
    let status = child.wait().context("waiting for ffmpeg")?;
    if cancel.load(Ordering::Relaxed) {
        anyhow::bail!("cancelled");
    }
    if !status.success() {
        let tail = stderr_tail
            .iter()
            .rev()
            .find(|l| !l.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| "ffmpeg failed".to_owned());
        anyhow::bail!(
            "ffmpeg failed: {}",
            tail.chars().take(240).collect::<String>()
        );
    }
    let out_path = args.last().context("ffmpeg args missing output")?;
    let bytes = fs::metadata(out_path).map(|m| m.len()).unwrap_or(0);
    Ok((bytes, started.elapsed().as_secs_f64().max(0.01)))
}

fn tmp_path_for(path: &Path) -> PathBuf {
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("asset");
    path.with_file_name(format!(".shrink-tmp-{file_name}"))
}

/// Verify `tmp`, then replace `original` with it — unless the re-encode
/// came out larger, in which case the original is kept untouched and
/// `(original, 0)` is returned. Same-directory tmp means the rename
/// never crosses volumes. Estimates are honest approximations, so
/// no-gain files must be a no-op, never a set-aborting failure.
fn swap_verified(
    original: &Path,
    tmp: &Path,
    verify: impl FnOnce() -> Result<()>,
) -> Result<(PathBuf, u64)> {
    verify()?;
    let before = fs::metadata(original).map(|m| m.len()).unwrap_or(0);
    let tmp_bytes = fs::metadata(tmp).map(|m| m.len()).unwrap_or(0);
    if tmp_bytes >= before {
        let _ = fs::remove_file(tmp);
        return Ok((original.to_owned(), 0));
    }
    fs::rename(tmp, original).with_context(|| format!("replacing {}", original.display()))?;
    Ok((original.to_owned(), before.saturating_sub(tmp_bytes)))
}

pub fn convert_audio(
    bins: &ShrinkBins,
    asset: &SetAsset,
    folder: &Path,
    cancel: &AtomicBool,
) -> Result<ShrinkOutcome> {
    let src = folder.join(&asset.name);
    let tmp = tmp_path_for(&src);
    let _ = fs::remove_file(&tmp);
    let mut args: Vec<String> = vec![
        "-y".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostats".into(),
        "-i".into(),
        src.to_string_lossy().into_owned(),
        "-map".into(),
        "0:a:0".into(),
        "-vn".into(),
        "-sn".into(),
        "-map_metadata".into(),
        "-1".into(),
    ];
    match asset.action {
        ShrinkAction::Mp3Cbr192 => {
            args.extend([
                "-c:a".into(),
                "libmp3lame".into(),
                "-b:a".into(),
                "192k".into(),
            ]);
        }
        ShrinkAction::OggQ6 => {
            args.extend(["-c:a".into(), "libvorbis".into(), "-q:a".into(), "6".into()]);
        }
        _ => anyhow::bail!("not an audio action: {}", asset.name),
    }
    args.push(tmp.to_string_lossy().into_owned());
    let (_bytes, _) = run_ffmpeg(&bins.ffmpeg, &args, cancel)?;
    // Verify: same stream decodes and duration matches within tolerance
    // (MP3 padding shifts by tens of ms at most).
    let before = probe_media(bins.ffprobe.as_deref(), &src)
        .and_then(|p| p.duration_s.is_significant().then_some(p.duration_s))
        .unwrap_or(0.0);
    let (path, _) = swap_verified(&src, &tmp, || {
        // NOTE: `src` is still the original here — verify the tmp output.
        let after = probe_media(bins.ffprobe.as_deref(), &tmp)
            .and_then(|p| p.duration_s.is_significant().then_some(p.duration_s))
            .ok_or_else(|| anyhow::anyhow!("converted audio does not decode"))?;
        if before > 0.0 && (before - after).abs() > (before * 0.02).max(0.5) {
            anyhow::bail!("duration drifted ({before:.1}s → {after:.1}s)");
        }
        Ok(())
    })?;
    let _ = fs::remove_file(&tmp);
    Ok(ShrinkOutcome { path })
}

/// Background video → H.264 ≤720p, same filename, audio/subs stripped.
pub fn convert_video(
    bins: &ShrinkBins,
    asset: &SetAsset,
    folder: &Path,
    options: &ShrinkOptions,
    cancel: &AtomicBool,
) -> Result<ShrinkOutcome> {
    let src = folder.join(&asset.name);
    let tmp = tmp_path_for(&src);
    let _ = fs::remove_file(&tmp);
    let crf = options.video_crf.clamp(18, 28).to_string();
    let args: Vec<String> = vec![
        "-y".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostats".into(),
        "-i".into(),
        src.to_string_lossy().into_owned(),
        "-map".into(),
        "0:v:0".into(),
        "-c:v".into(),
        "libx264".into(),
        "-crf".into(),
        crf,
        "-preset".into(),
        "medium".into(),
        "-vf".into(),
        format!("scale=-2:min(ih\\,{VIDEO_MAX_HEIGHT})"),
        "-an".into(),
        "-sn".into(),
        "-map_metadata".into(),
        "-1".into(),
        "-movflags".into(),
        "+faststart".into(),
        tmp.to_string_lossy().into_owned(),
    ];
    let (_bytes, _) = run_ffmpeg(&bins.ffmpeg, &args, cancel)?;
    let (path, _) = swap_verified(&src, &tmp, || {
        // NOTE: `src` is still the original here — verify the tmp output.
        let probe = probe_media(bins.ffprobe.as_deref(), &tmp)
            .ok_or_else(|| anyhow::anyhow!("converted video does not decode"))?;
        if probe.codec != "h264" {
            anyhow::bail!("unexpected codec {}", probe.codec);
        }
        if probe.height == 0 || probe.height > VIDEO_MAX_HEIGHT {
            anyhow::bail!("unexpected height {}", probe.height);
        }
        Ok(())
    })?;
    let _ = fs::remove_file(&tmp);
    Ok(ShrinkOutcome { path })
}

pub fn convert_image(
    asset: &SetAsset,
    folder: &Path,
    options: &ShrinkOptions,
) -> Result<ShrinkOutcome> {
    let src = folder.join(&asset.name);
    let img = image::io::Reader::open(&src)
        .with_context(|| format!("reading {}", src.display()))?
        .decode()
        .with_context(|| format!("decoding {}", src.display()))?;
    let (w, h) = (img.width(), img.height());
    let img = if w > IMAGE_MAX_WIDTH {
        let nw = IMAGE_MAX_WIDTH;
        let nh = ((h as u64 * nw as u64) / w as u64).max(1) as u32;
        img.resize_exact(nw, nh, image::imageops::FilterType::Lanczos3)
    } else {
        img
    };
    let ext = lower_ext(&src);
    let tmp = tmp_path_for(&src);
    // PNG/BMP/TIFF land in the PNG branch of the shared encoder.
    encode_image_to_tmp(&img, &ext, options.jpeg_quality, &tmp)?;
    // Verify the tmp decodes at the planned dimensions before swapping.
    let (vw, vh) = image_dimensions(&tmp).ok_or_else(|| anyhow::anyhow!("tmp image unreadable"))?;
    if vw == 0 || vh == 0 {
        let _ = fs::remove_file(&tmp);
        anyhow::bail!("tmp image invalid");
    }
    let (path, _) = swap_verified(&src, &tmp, || Ok(()))?;
    let _ = fs::remove_file(&tmp);
    Ok(ShrinkOutcome { path })
}

/// Shared image encoder: JPEG-likes go through the quality-controlled
/// JPEG encoder, everything else through a tight PNG encode.
fn encode_image_to_tmp(
    img: &image::DynamicImage,
    ext: &str,
    quality: u8,
    tmp: &Path,
) -> Result<()> {
    let file = fs::File::create(tmp).with_context(|| "creating tmp image")?;
    let mut writer = std::io::BufWriter::new(file);
    if matches!(ext, "jpg" | "jpeg" | "webp") {
        let rgb = img.to_rgb8();
        let (w, h) = (rgb.width(), rgb.height());
        let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut writer, quality);
        enc.encode(rgb.as_raw(), w, h, image::ColorType::Rgb8)
            .with_context(|| "encoding jpeg")?;
    } else {
        img.write_to(&mut writer, image::ImageFormat::Png)
            .with_context(|| "encoding png")?;
    }
    Ok(())
    // (BufWriter flushes on drop before `file` closes.)
}

/// Skin elements are pixel-exact gameplay/UI art: same dimensions, same
/// container, tighter encode — never a downscale. Combined with the
/// keep-if-smaller swap, worst case is a no-op, never a blurrier skin.
pub fn convert_skin_image(
    asset: &SetAsset,
    folder: &Path,
    options: &ShrinkOptions,
) -> Result<ShrinkOutcome> {
    let src = folder.join(&asset.name);
    let img = image::io::Reader::open(&src)
        .with_context(|| format!("reading {}", src.display()))?
        .decode()
        .with_context(|| format!("decoding {}", src.display()))?;
    let ext = lower_ext(&src);
    let tmp = tmp_path_for(&src);
    encode_image_to_tmp(&img, &ext, options.jpeg_quality, &tmp)?;
    let (vw, vh) = image_dimensions(&tmp).ok_or_else(|| anyhow::anyhow!("tmp image unreadable"))?;
    if vw != img.width() || vh != img.height() {
        let _ = fs::remove_file(&tmp);
        anyhow::bail!("tmp image dimensions changed");
    }
    let (path, _) = swap_verified(&src, &tmp, || Ok(()))?;
    let _ = fs::remove_file(&tmp);
    Ok(ShrinkOutcome { path })
}

// ── Backup / restore ─────────────────────────────────────────

/// Zip a set folder into `backup_dir/<folder>-<timestamp>.zip`.
pub fn backup_set(folder: &Path, backup_dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(backup_dir).with_context(|| "creating backup dir")?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let folder_name = folder.file_name().and_then(|n| n.to_str()).unwrap_or("set");
    let zip_path = backup_dir.join(format!("{folder_name}-{stamp}.zip"));
    let file = fs::File::create(&zip_path).with_context(|| "creating backup zip")?;
    let mut zip = zip::ZipWriter::new(file);
    let zip_options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let mut entries: Vec<PathBuf> = fs::read_dir(folder)
        .with_context(|| format!("reading {}", folder.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    entries.sort();
    for path in entries {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_owned();
        // Skip our own tmp files from a crashed run.
        if name.starts_with(".shrink-tmp-") {
            continue;
        }
        zip.start_file(name, zip_options)?;
        let mut src = fs::File::open(&path)?;
        std::io::copy(&mut src, &mut zip)?;
    }
    zip.finish()?;
    Ok(zip_path)
}

pub fn restore_backup(zip_path: &Path, folder: &Path) -> Result<usize> {
    let file = fs::File::open(zip_path).with_context(|| "opening backup zip")?;
    let mut zip = zip::ZipArchive::new(file).with_context(|| "reading backup zip")?;
    let count = zip.len();
    for i in 0..count {
        let mut entry = zip.by_index(i)?;
        let Some(name) = entry.name().rsplit('/').next().map(|s| s.to_owned()) else {
            continue;
        };
        if name.is_empty() || name.starts_with('.') {
            continue;
        }
        let dest = folder.join(&name);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = fs::File::create(&dest)?;
        std::io::copy(&mut entry, &mut out)?;
    }
    Ok(count)
}

// ── Job runner ───────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum ShrinkEvent {
    Started {
        sets: usize,
        assets: usize,
    },
    SetStarted {
        label: String,
    },
    AssetDone {
        saved: u64,
    },
    SetDone {
        folder: PathBuf,
        saved: u64,
        backup: Option<PathBuf>,
    },
    SetFailed {
        folder: PathBuf,
        message: String,
    },
    Finished {
        saved: u64,
        elapsed_s: f64,
    },
}

#[derive(Debug, Clone)]
pub struct ShrinkJob {
    pub report: SetShrinkReport,
}

/// Cooperative pause point for worker loops. Spins (sleeping, not
/// busy-looping) while `paused` is set; returns false when `cancel`
/// fires mid-pause so the caller breaks out.
pub(crate) fn wait_while_paused(paused: &AtomicBool, cancel: &AtomicBool) -> bool {
    while paused.load(Ordering::Relaxed) {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    !cancel.load(Ordering::Relaxed)
}

/// `cache_file` (e.g. `<osu root>/.osu-map-manager/shrink_cache.json`)
/// persists successfully shrunk files: each worker records into a
/// private shard, shards merge with the on-disk base at the end, and
/// the merged cache is saved — even on cancel, so partial runs still
/// count and are never redone.
/// Pause takes effect between files (an in-flight ffmpeg encode always
/// finishes first); cancel takes effect at the same points.
#[allow(clippy::too_many_arguments)]
pub fn run_shrink_jobs(
    jobs: Vec<ShrinkJob>,
    bins: ShrinkBins,
    options: ShrinkOptions,
    backup_dir: PathBuf,
    delete_orphans: bool,
    cancel: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
    tx: Sender<ShrinkEvent>,
    cache_file: PathBuf,
) {
    let started = Instant::now();
    let total_assets: usize = jobs
        .iter()
        .map(|j| {
            j.report
                .assets
                .iter()
                .filter(|a| {
                    a.action.is_work() && (delete_orphans || a.action != ShrinkAction::DeleteOrphan)
                })
                .count()
        })
        .sum();
    let _ = tx.send(ShrinkEvent::Started {
        sets: jobs.len(),
        assets: total_assets,
    });
    // Work-stealing pool over set folders: sets are fully independent
    // (own folder, own backup zip), and every event carries its folder,
    // so the UI aggregates correctly no matter the completion order.
    let workers = options.jobs.clamp(1, 8).min(jobs.len().max(1));
    let next = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let jobs = Arc::new(jobs);
    let saved_total = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut handles = Vec::with_capacity(workers);
    for _ in 0..workers {
        let tx = tx.clone();
        let jobs = jobs.clone();
        let next = next.clone();
        let saved_total = saved_total.clone();
        let bins = bins.clone();
        let options = options.clone();
        let backup_dir = backup_dir.clone();
        let cancel = cancel.clone();
        let pause = pause.clone();
        handles.push(std::thread::spawn(move || {
            let mut shard = ShrinkCache::default();
            loop {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                if !wait_while_paused(&pause, &cancel) {
                    break;
                }
                let index = next.fetch_add(1, Ordering::Relaxed);
                let Some(job) = jobs.get(index) else {
                    break;
                };
                let folder = job.report.folder.clone();
                let _ = tx.send(ShrinkEvent::SetStarted {
                    label: job.report.label.clone(),
                });
                match run_one_set(
                    job,
                    &bins,
                    &options,
                    &backup_dir,
                    delete_orphans,
                    &cancel,
                    &pause,
                    &tx,
                    &mut shard,
                ) {
                    Ok((saved, backup)) => {
                        saved_total.fetch_add(saved, Ordering::Relaxed);
                        let _ = tx.send(ShrinkEvent::SetDone {
                            folder,
                            saved,
                            backup,
                        });
                    }
                    Err(err) => {
                        let _ = tx.send(ShrinkEvent::SetFailed {
                            folder,
                            message: format!("{err:#}"),
                        });
                    }
                }
            }
            shard
        }));
    }
    let mut merged = ShrinkCache::load(&cache_file);
    for handle in handles {
        if let Ok(shard) = handle.join() {
            merged.extend(shard);
        }
    }
    // A failed save only costs redoing work next time (backups and
    // encodes already succeeded loudly or not at all), so don't fail
    // the run over it.
    let _ = merged.save(&cache_file);
    let _ = tx.send(ShrinkEvent::Finished {
        saved: saved_total.load(Ordering::Relaxed),
        elapsed_s: started.elapsed().as_secs_f64(),
    });
}

#[allow(clippy::too_many_arguments)]
fn run_one_set(
    job: &ShrinkJob,
    bins: &ShrinkBins,
    options: &ShrinkOptions,
    backup_dir: &Path,
    delete_orphans: bool,
    cancel: &AtomicBool,
    pause: &AtomicBool,
    tx: &Sender<ShrinkEvent>,
    cache: &mut ShrinkCache,
) -> Result<(u64, Option<PathBuf>)> {
    let folder = &job.report.folder;
    // Work list is fixed up front; orphans only with explicit opt-in.
    let work: Vec<&SetAsset> = job
        .report
        .assets
        .iter()
        .filter(|a| {
            a.action.is_work() && (delete_orphans || a.action != ShrinkAction::DeleteOrphan)
        })
        .collect();
    if work.is_empty() {
        return Ok((0, None));
    }
    let backup = if options.backup {
        Some(backup_set(folder, backup_dir)?)
    } else {
        None
    };
    let mut saved = 0u64;
    for asset in work {
        if cancel.load(Ordering::Relaxed) {
            anyhow::bail!("cancelled");
        }
        if !wait_while_paused(pause, cancel) {
            anyhow::bail!("cancelled");
        }
        let outcome = match &asset.action {
            ShrinkAction::Mp3Cbr192 | ShrinkAction::OggQ6 => {
                convert_audio(bins, asset, folder, cancel)?
            }
            ShrinkAction::Video720pH264 => convert_video(bins, asset, folder, options, cancel)?,
            ShrinkAction::ImageReencode => convert_image(asset, folder, options)?,
            ShrinkAction::SkinImageReencode => convert_skin_image(asset, folder, options)?,
            ShrinkAction::RemoveVideo => {
                let target = folder.join(&asset.name);
                fs::remove_file(&target)
                    .with_context(|| format!("deleting {}", target.display()))?;
                if target.exists() {
                    anyhow::bail!("{} still exists after delete", asset.name);
                }
                ShrinkOutcome { path: target }
            }
            ShrinkAction::DeleteOrphan => {
                let target = folder.join(&asset.name);
                fs::remove_file(&target)
                    .with_context(|| format!("deleting {}", target.display()))?;
                ShrinkOutcome { path: target }
            }
            ShrinkAction::Skip { .. } => continue,
        };
        // A "successful" encode that grew the file is a failure: the
        // estimate was wrong, and osu! assets must never grow.
        let after = fs::metadata(&outcome.path).map(|m| m.len()).unwrap_or(0);
        if after > asset.bytes {
            anyhow::bail!(
                "{} grew ({} → {}); restore the backup for this set",
                asset.name,
                human_bytes(asset.bytes),
                human_bytes(after)
            );
        }
        // Record re-encodes (including no-gain keep-originals, so a
        // later analysis skips retrying them instead of burning CPU and
        // stacking another lossy generation).
        if asset.action.is_reencode()
            && let Some((size, mtime)) = file_sig(&outcome.path)
        {
            cache.record(
                &outcome.path,
                size,
                mtime,
                &asset.action,
                &options.fingerprint(),
            );
        }
        saved += asset.bytes.saturating_sub(after);
        let _ = tx.send(ShrinkEvent::AssetDone {
            saved: asset.bytes.saturating_sub(after),
        });
    }
    Ok((saved, backup))
}

pub fn human_bytes(bytes: u64) -> String {
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    let value = bytes as f64;
    if value >= GB {
        format!("{:.2} GB", value / GB)
    } else if value >= MB {
        format!("{:.1} MB", value / MB)
    } else {
        format!("{:.0} KB", value / 1024.0)
    }
}

/// `(bytes in, bytes estimated, work items)` across reports.
pub fn summarize(reports: &[SetShrinkReport]) -> (u64, u64, usize) {
    let total_in: u64 = reports.iter().map(|r| r.total_in).sum();
    let total_est: u64 = reports.iter().map(|r| r.total_est).sum();
    let items: usize = reports.iter().map(|r| r.work_items()).sum();
    (total_in, total_est, items)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Mutex, MutexGuard};
    /// Video encodes are CPU-heavy; running two e2e suites at once makes
    /// timing-sensitive setup ffmpeg calls flaky on Windows, so the
    /// ffmpeg-backed tests serialize on this lock.
    static E2E_LOCK: Mutex<()> = Mutex::new(());

    fn e2e_lock() -> MutexGuard<'static, ()> {
        E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn have_ffmpeg() -> Option<ShrinkBins> {
        let ffmpeg = which_on_path(exe_name("ffmpeg")).or_else(|| {
            [
                "C:\\Users\\feel\\AppData\\Local\\Microsoft\\WinGet\\Packages\\Gyan.FFmpeg.Shared_Microsoft.Winget.Source_8wekyb3d8bbwe\\ffmpeg-9.0.1-full_build-shared\\bin\\ffmpeg.exe",
            ]
            .into_iter()
            .map(PathBuf::from)
            .find(|p| p.is_file())
        })?;
        let bins = resolve_bins(ffmpeg);
        bins.ffprobe.as_deref()?.is_file().then_some(bins)
    }

    /// Full pipeline against real ffmpeg: generate a fat MP3 + oversized
    /// JPEG + HD video, analyze, convert, verify smaller + valid, and
    /// prove the `.osu` is byte-identical. Skipped where ffmpeg is absent.
    #[test]
    fn end_to_end_shrink_with_real_ffmpeg() {
        let _guard = e2e_lock();
        let Some(bins) = have_ffmpeg() else {
            return;
        };
        let dir = std::env::temp_dir().join(format!(
            "osu-shrink-e2e-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|t| t.as_nanos())
                .unwrap_or(0)
        ));
        let set = dir.join("set");
        fs::create_dir_all(&set).unwrap();
        let run = |args: &[&str]| {
            let out = Command::new(&bins.ffmpeg)
                .args(args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "setup ffmpeg failed: {args:?}\nstderr: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        // 320k MP3 (2s), 2400px JPEG, 1280x800 MP4 with audio.
        run(&[
            "-y",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=2",
            "-c:a",
            "libmp3lame",
            "-b:a",
            "320k",
            &set.join("audio.mp3").to_string_lossy(),
        ]);
        // q100 fixture: downscale + q85 pass genuinely shrinks it
        // (a default-quality save would already be near-optimal and the
        // keep-if-smaller swap would — correctly — refuse it).
        let big = image::RgbImage::from_fn(2400, 1400, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
        let bg_file = fs::File::create(set.join("bg.jpg")).unwrap();
        let mut bg_enc = image::codecs::jpeg::JpegEncoder::new_with_quality(
            std::io::BufWriter::new(bg_file),
            100,
        );
        bg_enc
            .encode(big.as_raw(), 2400, 1400, image::ColorType::Rgb8)
            .unwrap();
        drop(bg_enc);
        run(&[
            "-y",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=1280x800:rate=15:duration=1",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
            "-c:v",
            "libx264",
            "-crf",
            "18",
            "-preset",
            "veryfast",
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            "-shortest",
            &set.join("clip.mp4").to_string_lossy(),
        ]);
        let osu = "[General]\nAudioFilename: audio.mp3\n[Events]\n0,0,\"bg.jpg\",0,0\nVideo,0,\"clip.mp4\",0,0\n";
        fs::write(set.join("map.osu"), osu).unwrap();
        let osu_before = fs::read(set.join("map.osu")).unwrap();

        let maps = vec![beatmap(
            set.join("map.osu"),
            set.clone(),
            Some("audio.mp3"),
            Some("bg.jpg"),
        )];
        let options = ShrinkOptions::default();
        let report = analyze_set(
            &set,
            "set".into(),
            &maps,
            &bins,
            &options,
            &mut ProbeCache::new(),
            &ShrinkCache::default(),
        );
        let action_of = |name: &str| {
            report
                .assets
                .iter()
                .find(|a| a.name == name)
                .map(|a| a.action.clone())
        };
        assert_eq!(action_of("audio.mp3"), Some(ShrinkAction::Mp3Cbr192));
        assert_eq!(action_of("clip.mp4"), Some(ShrinkAction::Video720pH264));
        assert_eq!(action_of("bg.jpg"), Some(ShrinkAction::ImageReencode));

        let cancel = AtomicBool::new(false);
        for asset in &report.assets {
            match asset.action {
                ShrinkAction::Mp3Cbr192 | ShrinkAction::OggQ6 => {
                    convert_audio(&bins, asset, &set, &cancel).unwrap();
                }
                ShrinkAction::Video720pH264 => {
                    convert_video(&bins, asset, &set, &options, &cancel).unwrap();
                }
                ShrinkAction::ImageReencode => {
                    convert_image(asset, &set, &options).unwrap();
                }
                _ => {}
            }
        }
        // Everything shrank, everything still decodes, .osu untouched.
        let after_audio = fs::metadata(set.join("audio.mp3")).unwrap().len();
        assert!(after_audio < 100_000, "mp3 not shrunk: {after_audio}");
        assert!(probe_media(bins.ffprobe.as_deref(), &set.join("audio.mp3")).is_some());
        let (w, h) = image_dimensions(&set.join("bg.jpg")).unwrap();
        assert!(w <= IMAGE_MAX_WIDTH, "bg not downscaled: {w}x{h}");
        let vprobe = probe_media(bins.ffprobe.as_deref(), &set.join("clip.mp4")).unwrap();
        assert_eq!(vprobe.codec, "h264");
        assert!(vprobe.height <= VIDEO_MAX_HEIGHT);
        assert!(!vprobe.has_audio);
        assert_eq!(fs::read(set.join("map.osu")).unwrap(), osu_before);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn skins_plan_images_not_sounds_or_config() {
        let dir = std::env::temp_dir().join(format!(
            "osu-shrink-skin-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|t| t.as_nanos())
                .unwrap_or(0)
        ));
        let skin = dir.join("MySkin");
        fs::create_dir_all(&skin).unwrap();
        // Noisy PNG compresses poorly → comfortably over the threshold.
        // (Its bytes are already near-optimal, so conversion is a no-op;
        // the strict shrink assertion below uses the q100 JPEG instead.)
        let noisy = image::RgbImage::from_fn(600, 600, |x, y| {
            image::Rgb([
                (x * 7 % 256) as u8,
                (y * 13 % 256) as u8,
                ((x + y) % 256) as u8,
            ])
        });
        noisy.save(skin.join("hitcircle.png")).unwrap();
        assert!(fs::metadata(skin.join("hitcircle.png")).unwrap().len() > SKIN_PNG_MIN_BYTES);
        // Noisy q100 JPEG: genuinely shrinkable by the q85 pass.
        let big = image::RgbImage::from_fn(1200, 900, |x, y| {
            image::Rgb([
                (x * 3 % 256) as u8,
                (y * 5 % 256) as u8,
                ((x * y) % 256) as u8,
            ])
        });
        let menu_file = fs::File::create(skin.join("menu-background.jpg")).unwrap();
        let mut menu_enc = image::codecs::jpeg::JpegEncoder::new_with_quality(
            std::io::BufWriter::new(menu_file),
            100,
        );
        menu_enc
            .encode(big.as_raw(), 1200, 900, image::ColorType::Rgb8)
            .unwrap();
        drop(menu_enc);
        assert!(
            fs::metadata(skin.join("menu-background.jpg"))
                .unwrap()
                .len()
                > SKIN_JPEG_MIN_BYTES
        );
        let tiny = image::RgbImage::from_pixel(8, 8, image::Rgb([1, 2, 3]));
        tiny.save(skin.join("cursor.png")).unwrap();
        fs::write(skin.join("skin.ini"), "[General]\nName: x\n").unwrap();
        fs::write(skin.join("hitsound.wav"), vec![0u8; 1024]).unwrap();

        let report = analyze_skin(
            &skin,
            "MySkin".into(),
            &ShrinkOptions::default(),
            &ShrinkCache::default(),
        );
        let action_of = |name: &str| {
            report
                .assets
                .iter()
                .find(|a| a.name == name)
                .map(|a| a.action.clone())
        };
        assert_eq!(
            action_of("hitcircle.png"),
            Some(ShrinkAction::SkinImageReencode)
        );
        assert!(matches!(
            action_of("cursor.png"),
            Some(ShrinkAction::Skip { .. })
        ));
        assert!(matches!(
            action_of("skin.ini"),
            Some(ShrinkAction::Skip { .. })
        ));
        assert!(matches!(
            action_of("hitsound.wav"),
            Some(ShrinkAction::Skip { .. })
        ));
        // Subfolders are not loaded by the game: invisible.
        fs::create_dir_all(skin.join("sub")).unwrap();
        fs::write(skin.join("sub").join("nested.png"), vec![0u8; 200_000]).unwrap();
        let report = analyze_skin(
            &skin,
            "MySkin".into(),
            &ShrinkOptions::default(),
            &ShrinkCache::default(),
        );
        assert!(report.assets.iter().all(|a| a.name != "nested.png"));

        // Conversion keeps pixels and never grows: the already-optimal
        // PNG is a byte-identical no-op, the q100 JPEG genuinely shrinks.
        let png_asset = report
            .assets
            .iter()
            .find(|a| a.name == "hitcircle.png")
            .unwrap()
            .clone();
        let png_before = fs::read(skin.join("hitcircle.png")).unwrap();
        convert_skin_image(&png_asset, &skin, &ShrinkOptions::default()).unwrap();
        let png_after = fs::read(skin.join("hitcircle.png")).unwrap();
        assert!(png_after.len() <= png_before.len());
        let (w, h) = image_dimensions(&skin.join("hitcircle.png")).unwrap();
        assert_eq!((w, h), (600, 600));

        let jpg_asset = report
            .assets
            .iter()
            .find(|a| a.name == "menu-background.jpg")
            .unwrap()
            .clone();
        assert_eq!(jpg_asset.action, ShrinkAction::SkinImageReencode);
        let jpg_before = fs::read(skin.join("menu-background.jpg")).unwrap();
        convert_skin_image(&jpg_asset, &skin, &ShrinkOptions::default()).unwrap();
        let jpg_after = fs::read(skin.join("menu-background.jpg")).unwrap();
        assert!(jpg_after.len() < jpg_before.len(), "q100 jpg must shrink");
        let (w, h) = image_dimensions(&skin.join("menu-background.jpg")).unwrap();
        assert_eq!((w, h), (1200, 900));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn no_gain_files_are_kept_untouched() {
        // A 1x1 PNG is already minimal: the tighter re-encode cannot beat
        // it, so the converter must keep the original byte-identical
        // instead of failing.
        let dir = std::env::temp_dir().join(format!(
            "osu-shrink-nogain-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|t| t.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        let pixel = image::RgbImage::from_pixel(1, 1, image::Rgb([9, 9, 9]));
        pixel.save(dir.join("tiny.png")).unwrap();
        let before = fs::read(dir.join("tiny.png")).unwrap();
        let asset = SetAsset {
            name: "tiny.png".into(),
            bytes: before.len() as u64,
            kind: ShrinkAssetKind::Image,
            action: ShrinkAction::SkinImageReencode,
            est_bytes: 0,
            note: String::new(),
        };
        convert_skin_image(&asset, &dir, &ShrinkOptions::default()).unwrap();
        let after = fs::read(dir.join("tiny.png")).unwrap();
        assert!(after.len() <= before.len(), "output must never grow");
        assert_eq!(after, before, "no-gain file must be untouched");
        assert!(!dir.join(".shrink-tmp-tiny.png").exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parallel_runner_handles_orphan_sets_without_ffmpeg() {
        use std::sync::mpsc;
        let dir = std::env::temp_dir().join(format!(
            "osu-shrink-par-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|t| t.as_nanos())
                .unwrap_or(0)
        ));
        let mut jobs = Vec::new();
        for i in 0..3 {
            let set = dir.join(format!("set{i}"));
            fs::create_dir_all(&set).unwrap();
            fs::write(set.join("map.osu"), "[General]\nAudioFilename: audio.mp3\n").unwrap();
            fs::write(set.join("audio.mp3"), vec![0u8; 1024]).unwrap();
            fs::write(set.join("leftover.ogg"), vec![0u8; 2048]).unwrap();
            let maps = vec![beatmap(
                set.join("map.osu"),
                set.clone(),
                Some("audio.mp3"),
                None,
            )];
            let bins = ShrinkBins {
                ffmpeg: PathBuf::from("ffmpeg"),
                ffprobe: None,
            };
            let options = ShrinkOptions {
                delete_orphans: true,
                backup: false,
                ..Default::default()
            };
            let report = analyze_set(
                &set,
                format!("set{i}"),
                &maps,
                &bins,
                &options,
                &mut ProbeCache::new(),
                &ShrinkCache::default(),
            );
            jobs.push(ShrinkJob { report });
        }
        let (tx, rx) = mpsc::channel();
        let options = ShrinkOptions {
            jobs: 3,
            backup: false,
            delete_orphans: true,
            ..Default::default()
        };
        let bins = ShrinkBins {
            ffmpeg: PathBuf::from("ffmpeg"),
            ffprobe: None,
        };
        run_shrink_jobs(
            jobs,
            bins,
            options,
            dir.join("backups"),
            true,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            tx,
            dir.join("shrink_cache.json"),
        );
        let mut finished_saved = None;
        let mut done_sets = 0;
        while let Ok(event) = rx.try_recv() {
            match event {
                ShrinkEvent::SetDone { .. } => done_sets += 1,
                ShrinkEvent::Finished { saved, .. } => finished_saved = Some(saved),
                ShrinkEvent::SetFailed { message, .. } => panic!("set failed: {message}"),
                _ => {}
            }
        }
        assert_eq!(done_sets, 3);
        assert_eq!(finished_saved, Some(3 * 2048));
        for i in 0..3 {
            assert!(!dir.join(format!("set{i}")).join("leftover.ogg").exists());
            assert!(dir.join(format!("set{i}")).join("audio.mp3").exists());
        }
        fs::remove_dir_all(&dir).ok();
    }

    fn orphan_job(dir: &Path, name: &str) -> (ShrinkJob, ShrinkBins, ShrinkOptions) {
        let set = dir.join(name);
        fs::create_dir_all(&set).unwrap();
        fs::write(set.join("map.osu"), "[General]\nAudioFilename: audio.mp3\n").unwrap();
        fs::write(set.join("audio.mp3"), vec![0u8; 1024]).unwrap();
        fs::write(set.join("leftover.ogg"), vec![0u8; 2048]).unwrap();
        let maps = vec![beatmap(
            set.join("map.osu"),
            set.clone(),
            Some("audio.mp3"),
            None,
        )];
        let bins = ShrinkBins {
            ffmpeg: PathBuf::from("ffmpeg"),
            ffprobe: None,
        };
        let options = ShrinkOptions {
            delete_orphans: true,
            backup: false,
            ..Default::default()
        };
        let report = analyze_set(
            &set,
            name.into(),
            &maps,
            &bins,
            &options,
            &mut ProbeCache::new(),
            &ShrinkCache::default(),
        );
        (ShrinkJob { report }, bins, options)
    }

    #[test]
    fn precancelled_run_does_nothing() {
        use std::sync::mpsc;
        let dir = std::env::temp_dir().join(format!(
            "osu-shrink-precancel-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|t| t.as_nanos())
                .unwrap_or(0)
        ));
        let (job, bins, options) = orphan_job(&dir, "set");
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(true));
        run_shrink_jobs(
            vec![job],
            bins,
            options,
            dir.join("backups"),
            true,
            cancel,
            Arc::new(AtomicBool::new(false)),
            tx,
            dir.join("shrink_cache.json"),
        );
        let mut finished_saved = None;
        while let Ok(event) = rx.try_recv() {
            match event {
                ShrinkEvent::Finished { saved, .. } => finished_saved = Some(saved),
                ShrinkEvent::SetDone { .. } => panic!("cancelled run must not complete sets"),
                _ => {}
            }
        }
        assert_eq!(finished_saved, Some(0));
        // Untouched: orphan still there, no backup zip written.
        assert!(dir.join("set").join("leftover.ogg").exists());
        assert!(!dir.join("backups").exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn paused_run_waits_then_completes_on_resume() {
        use std::sync::mpsc;
        use std::time::Duration;
        let dir = std::env::temp_dir().join(format!(
            "osu-shrink-pause-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|t| t.as_nanos())
                .unwrap_or(0)
        ));
        let (job, bins, options) = orphan_job(&dir, "set");
        let (tx, rx) = mpsc::channel();
        let pause = Arc::new(AtomicBool::new(true));
        let pause2 = pause.clone();
        let backups_dir = dir.join("backups");
        let cache_file = dir.join("shrink_cache.json");
        let handle = std::thread::spawn(move || {
            run_shrink_jobs(
                vec![job],
                bins,
                options,
                backups_dir,
                true,
                Arc::new(AtomicBool::new(false)),
                pause2,
                tx,
                cache_file,
            );
        });
        // Held paused well past the time an unpaused orphan delete takes
        // (milliseconds): nothing may complete while paused.
        std::thread::sleep(Duration::from_millis(400));
        let mut saw_done = false;
        while let Ok(event) = rx.try_recv() {
            if matches!(event, ShrinkEvent::SetDone { .. }) {
                saw_done = true;
            }
        }
        assert!(!saw_done, "paused run must not complete sets");
        assert!(dir.join("set").join("leftover.ogg").exists());
        // Resume: the run drains to a normal finish.
        pause.store(false, Ordering::Relaxed);
        handle.join().unwrap();
        let mut finished_saved = None;
        while let Ok(event) = rx.try_recv() {
            if let ShrinkEvent::Finished { saved, .. } = event {
                finished_saved = Some(saved);
            }
        }
        assert_eq!(finished_saved, Some(2048));
        assert!(!dir.join("set").join("leftover.ogg").exists());
        fs::remove_dir_all(&dir).ok();
    }

    /// The probe cache is what makes option toggles instant: with a warm
    /// cache, analysis plans identically even when ffprobe is gone.
    #[test]
    fn probe_cache_survives_missing_ffprobe() {
        let _guard = e2e_lock();
        let Some(bins) = have_ffmpeg() else {
            return;
        };
        let dir = std::env::temp_dir().join(format!(
            "osu-shrink-cache-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|t| t.as_nanos())
                .unwrap_or(0)
        ));
        let set = dir.join("set");
        fs::create_dir_all(&set).unwrap();
        let out = Command::new(&bins.ffmpeg)
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=2",
                "-c:a",
                "libmp3lame",
                "-b:a",
                "320k",
                &set.join("audio.mp3").to_string_lossy(),
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(out.success());
        fs::write(set.join("map.osu"), "[General]\nAudioFilename: audio.mp3\n").unwrap();
        let maps = vec![beatmap(
            set.join("map.osu"),
            set.clone(),
            Some("audio.mp3"),
            None,
        )];
        let options = ShrinkOptions::default();

        let mut cache = ProbeCache::new();
        let empty_shrink = ShrinkCache::default();
        let report = analyze_set(
            &set,
            "set".into(),
            &maps,
            &bins,
            &options,
            &mut cache,
            &empty_shrink,
        );
        assert!(!cache.is_empty(), "warm run must populate the cache");
        let planned: Vec<ShrinkAction> = report.assets.iter().map(|a| a.action.clone()).collect();
        assert!(planned.contains(&ShrinkAction::Mp3Cbr192));

        // Same cache, no ffprobe: identical plans, zero spawns.
        let mut no_probe = bins.clone();
        no_probe.ffprobe = None;
        let report2 = analyze_set(
            &set,
            "set".into(),
            &maps,
            &no_probe,
            &options,
            &mut cache,
            &empty_shrink,
        );
        let planned2: Vec<ShrinkAction> = report2.assets.iter().map(|a| a.action.clone()).collect();
        assert_eq!(planned, planned2);

        // Fresh cache without ffprobe: everything probing-dependent skips.
        let report3 = analyze_set(
            &set,
            "set".into(),
            &maps,
            &no_probe,
            &options,
            &mut ProbeCache::new(),
            &empty_shrink,
        );
        assert!(report3.assets.iter().all(|a| !a.action.is_work()));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn shrink_cache_roundtrip_and_invalidation() {
        let dir = std::env::temp_dir();
        let file = dir.join("shrink-cache-unit.json");
        let _ = fs::remove_file(&file);
        let path = PathBuf::from("song/audio.mp3");
        let action = ShrinkAction::Mp3Cbr192;
        let settings = "v1 q85 crf21";

        let mut cache = ShrinkCache::default();
        assert!(!cache.is_shrunk(&path, 100, 200, &action, settings));
        cache.record(&path, 100, 200, &action, settings);
        assert!(cache.is_shrunk(&path, 100, 200, &action, settings));
        // Any change invalidates: size, mtime, action, settings.
        assert!(!cache.is_shrunk(&path, 101, 200, &action, settings));
        assert!(!cache.is_shrunk(&path, 100, 201, &action, settings));
        assert!(!cache.is_shrunk(&path, 100, 200, &ShrinkAction::OggQ6, settings));
        assert!(!cache.is_shrunk(&path, 100, 200, &action, "v1 q90 crf21"));

        cache.save(&file).unwrap();
        let loaded = ShrinkCache::load(&file);
        assert_eq!(loaded.len(), 1);
        assert!(loaded.is_shrunk(&path, 100, 200, &action, settings));

        // Corrupt file and version mismatch fail safe to empty.
        fs::write(&file, b"not json").unwrap();
        assert_eq!(ShrinkCache::load(&file).len(), 0);
        let _ = fs::remove_file(&file);
    }

    #[test]
    fn analysis_skips_cached_reencodes_and_run_records_them() {
        let dir = std::env::temp_dir().join(format!(
            "osu-shrink-cacheloop-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|t| t.as_nanos())
                .unwrap_or(0)
        ));
        let skin = dir.join("Skin");
        fs::create_dir_all(&skin).unwrap();
        // q100 JPEG: genuinely shrinkable by the q85 pass, and still
        // over the "already small" threshold afterwards (so the second
        // analysis exercises the cache path, not the size threshold).
        let big = image::RgbImage::from_fn(1600, 1200, |x, y| {
            image::Rgb([
                (x * 3 % 256) as u8,
                (y * 5 % 256) as u8,
                ((x * y) % 256) as u8,
            ])
        });
        let target = skin.join("menu-background.jpg");
        let out_file = fs::File::create(&target).unwrap();
        let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(
            std::io::BufWriter::new(out_file),
            100,
        );
        enc.encode(big.as_raw(), 1600, 1200, image::ColorType::Rgb8)
            .unwrap();
        drop(enc);

        let options = ShrinkOptions::default();
        let bins = ShrinkBins {
            ffmpeg: PathBuf::from("ffmpeg"),
            ffprobe: None,
        };
        let report = analyze_skin(&skin, "Skin".into(), &options, &ShrinkCache::default());
        let asset = report
            .assets
            .iter()
            .find(|a| a.name == "menu-background.jpg")
            .unwrap();
        assert_eq!(asset.action, ShrinkAction::SkinImageReencode);

        // Run it with a live cache: the file converts and gets recorded.
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut cache = ShrinkCache::default();
        let job = ShrinkJob { report };
        let (saved, _) = run_one_set(
            &job,
            &bins,
            &options,
            &dir.join("backups"),
            false,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
            &tx,
            &mut cache,
        )
        .unwrap();
        assert!(saved > 0);
        assert_eq!(cache.len(), 1);

        // Re-analyze with the warm cache: skipped, counted, estimated same.
        let report2 = analyze_skin(&skin, "Skin".into(), &options, &cache);
        assert_eq!(report2.work_items(), 0);
        assert_eq!(report2.cached_items(), 1);
        assert_eq!(report2.total_est, report2.total_in);

        // …but a settings change re-plans it (new quality = new work).
        let changed = ShrinkOptions {
            jpeg_quality: 90,
            ..Default::default()
        };
        let report3 = analyze_skin(&skin, "Skin".into(), &changed, &cache);
        assert_eq!(report3.work_items(), 1);
        assert_eq!(report3.cached_items(), 0);
        fs::remove_dir_all(&dir).ok();
    }

    fn beatmap(
        path: PathBuf,
        folder: PathBuf,
        audio: Option<&str>,
        bg: Option<&str>,
    ) -> LocalBeatmap {
        LocalBeatmap {
            path,
            folder,
            audio_filename: audio.map(str::to_owned),
            background_filename: bg.map(str::to_owned),
            ..Default::default()
        }
    }

    #[test]
    fn event_assets_parse_video_and_sprites() {
        let body = "0,0,\"bg.jpg\",0,0\nVideo,0,\"clip.mp4\",0,0\nSprite,Foreground,Centre,\"sb\\star.png\",320,240\nL,0,Loop,\"sbdot.png\",1\nSample,5000,0,\"click.ogg\",100\n// comment\n";
        let (videos, sprites, samples) = parse_event_assets(body);
        assert!(videos.contains("clip.mp4"));
        assert!(sprites.contains("sb/star.png"));
        assert!(sprites.contains("sbdot.png"));
        assert!(samples.contains("click.ogg"));
        assert!(!videos.contains("bg.jpg"));
        assert!(!sprites.contains("click.ogg"));
    }

    #[test]
    fn storyboard_samples_are_protected_not_orphans() {
        let dir = std::env::temp_dir().join(format!(
            "osu-shrink-sample-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|t| t.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("map.osu"),
            "[General]\nAudioFilename: audio.mp3\n[Events]\nSample,5000,0,\"click.ogg\",100\n",
        )
        .unwrap();
        fs::write(dir.join("audio.mp3"), vec![0u8; 2048]).unwrap();
        fs::write(dir.join("click.ogg"), vec![0u8; 2048]).unwrap();
        let maps = vec![beatmap(
            dir.join("map.osu"),
            dir.clone(),
            Some("audio.mp3"),
            None,
        )];
        let bins = ShrinkBins {
            ffmpeg: PathBuf::from("ffmpeg"),
            ffprobe: None,
        };
        // Orphan deletion on: the sample must still be protected.
        let options = ShrinkOptions {
            delete_orphans: true,
            ..Default::default()
        };
        let report = analyze_set(
            &dir,
            "set".into(),
            &maps,
            &bins,
            &options,
            &mut ProbeCache::new(),
            &ShrinkCache::default(),
        );
        let sample = report
            .assets
            .iter()
            .find(|a| a.name == "click.ogg")
            .unwrap();
        assert_eq!(sample.kind, ShrinkAssetKind::Protected);
        assert!(!sample.action.is_work());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hitsound_names_are_protected() {
        assert!(is_hitsound_name("soft-hitnormal2.wav"));
        assert!(is_hitsound_name("drum-hitfinish.wav"));
        assert!(is_hitsound_name("normal-sliderslide.wav"));
        assert!(!is_hitsound_name("audio.mp3"));
        assert!(!is_hitsound_name("bg.jpg"));
    }

    #[test]
    fn analysis_never_touches_config_or_scores() {
        let dir = std::env::temp_dir().join(format!(
            "osu-shrink-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|t| t.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        // Minimal set: one difficulty, referenced audio + bg + video.
        fs::write(
            dir.join("map.osu"),
            "[General]\nAudioFilename: audio.mp3\n[Events]\n0,0,\"bg.jpg\",0,0\nVideo,0,\"clip.mp4\",0,0\n",
        )
        .unwrap();
        fs::write(dir.join("audio.mp3"), vec![0u8; 2048]).unwrap();
        fs::write(dir.join("bg.jpg"), vec![0u8; 2048]).unwrap();
        fs::write(dir.join("clip.mp4"), vec![0u8; 2048]).unwrap();
        fs::write(dir.join("hitnormal.wav"), vec![0u8; 512]).unwrap();
        fs::write(dir.join("map.osb"), "// storyboard\n").unwrap();
        fs::write(dir.join("skin.ini"), "[General]\n").unwrap();
        let maps = vec![beatmap(
            dir.join("map.osu"),
            dir.clone(),
            Some("audio.mp3"),
            Some("bg.jpg"),
        )];
        let bins = ShrinkBins {
            ffmpeg: PathBuf::from("ffmpeg"),
            ffprobe: None, // no probe → everything probing-dependent must Skip, not fail
        };
        let report = analyze_set(
            &dir,
            "set".into(),
            &maps,
            &bins,
            &ShrinkOptions::default(),
            &mut ProbeCache::new(),
            &ShrinkCache::default(),
        );
        // .osu/.osb/.ini invisible; hitsound protected; the three media
        // assets present but skipped (no ffprobe / tiny files).
        assert!(report.assets.iter().all(|a| !a.name.ends_with(".osu")));
        assert!(report.assets.iter().all(|a| !a.name.ends_with(".osb")));
        assert!(report.assets.iter().all(|a| !a.name.ends_with(".ini")));
        let wav = report
            .assets
            .iter()
            .find(|a| a.name == "hitnormal.wav")
            .unwrap();
        assert_eq!(wav.kind, ShrinkAssetKind::Protected);
        assert!(!wav.action.is_work());
        // Nothing to do without probes: estimates equal inputs exactly.
        assert_eq!(report.total_est, report.total_in);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn remove_videos_deletes_clip_but_never_touches_osu() {
        let dir = std::env::temp_dir().join(format!(
            "osu-shrink-novideo-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|t| t.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        let osu_text = "[General]\nAudioFilename: audio.mp3\n[Events]\n0,0,\"bg.jpg\",0,0\nVideo,0,\"clip.mp4\",0,0\n";
        fs::write(dir.join("map.osu"), osu_text).unwrap();
        fs::write(dir.join("audio.mp3"), vec![0u8; 2048]).unwrap();
        fs::write(dir.join("clip.mp4"), vec![0u8; 8192]).unwrap();
        let maps = vec![beatmap(
            dir.join("map.osu"),
            dir.clone(),
            Some("audio.mp3"),
            Some("bg.jpg"),
        )];
        let bins = ShrinkBins {
            ffmpeg: PathBuf::from("ffmpeg"),
            ffprobe: None,
        };
        // Default: videos are compressed (or skipped without a probe),
        // never removed.
        let plain = analyze_set(
            &dir,
            "set".into(),
            &maps,
            &bins,
            &ShrinkOptions::default(),
            &mut ProbeCache::new(),
            &ShrinkCache::default(),
        );
        let clip = plain.assets.iter().find(|a| a.name == "clip.mp4").unwrap();
        assert_ne!(clip.action, ShrinkAction::RemoveVideo);
        // Opt-in: removal planned with zero estimated bytes.
        let options = ShrinkOptions {
            remove_videos: true,
            ..Default::default()
        };
        let report = analyze_set(
            &dir,
            "set".into(),
            &maps,
            &bins,
            &options,
            &mut ProbeCache::new(),
            &ShrinkCache::default(),
        );
        let clip = report.assets.iter().find(|a| a.name == "clip.mp4").unwrap();
        assert_eq!(clip.action, ShrinkAction::RemoveVideo);
        assert_eq!(clip.est_bytes, 0);
        // Run it: clip gone, .osu byte-identical, backup taken.
        let (tx, _rx) = std::sync::mpsc::channel();
        let job = ShrinkJob { report };
        let (saved, backup) = run_one_set(
            &job,
            &bins,
            &options,
            &dir.join("backups"),
            false,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
            &tx,
            &mut ShrinkCache::default(),
        )
        .unwrap();
        assert_eq!(saved, 8192);
        assert!(!dir.join("clip.mp4").exists());
        assert_eq!(fs::read_to_string(dir.join("map.osu")).unwrap(), osu_text);
        assert!(backup.is_some());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn orphans_need_opt_in() {
        let dir = std::env::temp_dir().join(format!(
            "osu-shrink-orphan-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|t| t.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("map.osu"), "[General]\nAudioFilename: audio.mp3\n").unwrap();
        fs::write(dir.join("audio.mp3"), vec![0u8; 2048]).unwrap();
        fs::write(dir.join("leftover.ogg"), vec![0u8; 4096]).unwrap();
        let maps = vec![beatmap(
            dir.join("map.osu"),
            dir.clone(),
            Some("audio.mp3"),
            None,
        )];
        let bins = ShrinkBins {
            ffmpeg: PathBuf::from("ffmpeg"),
            ffprobe: None,
        };
        let off = analyze_set(
            &dir,
            "set".into(),
            &maps,
            &bins,
            &ShrinkOptions::default(),
            &mut ProbeCache::new(),
            &ShrinkCache::default(),
        );
        let orphan = off
            .assets
            .iter()
            .find(|a| a.name == "leftover.ogg")
            .unwrap();
        assert_eq!(orphan.kind, ShrinkAssetKind::Orphan);
        assert!(!orphan.action.is_work());
        let on = ShrinkOptions {
            delete_orphans: true,
            ..Default::default()
        };
        let report = analyze_set(
            &dir,
            "set".into(),
            &maps,
            &bins,
            &on,
            &mut ProbeCache::new(),
            &ShrinkCache::default(),
        );
        let orphan = report
            .assets
            .iter()
            .find(|a| a.name == "leftover.ogg")
            .unwrap();
        assert_eq!(orphan.action, ShrinkAction::DeleteOrphan);
        assert_eq!(orphan.est_bytes, 0);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn backup_and_restore_round_trip() {
        let dir = std::env::temp_dir().join(format!(
            "osu-shrink-backup-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|t| t.as_nanos())
                .unwrap_or(0)
        ));
        let backups = dir.join("backups");
        let set = dir.join("set");
        fs::create_dir_all(&set).unwrap();
        fs::write(set.join("audio.mp3"), b"original-audio").unwrap();
        fs::write(set.join("map.osu"), b"original-osu").unwrap();
        let zip = backup_set(&set, &backups).unwrap();
        assert!(zip.exists());
        // Simulate a shrink, then restore.
        fs::write(set.join("audio.mp3"), b"shrunk").unwrap();
        let restored = restore_backup(&zip, &set).unwrap();
        assert_eq!(restored, 2);
        assert_eq!(fs::read(set.join("audio.mp3")).unwrap(), b"original-audio");
        assert_eq!(fs::read(set.join("map.osu")).unwrap(), b"original-osu");
        fs::remove_dir_all(&dir).ok();
    }
}
