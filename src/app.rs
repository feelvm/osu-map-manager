use crate::{
    app_update::{self, AppRelease},
    collection,
    extras::{self, ExtrasEvent, ExtrasManifest, RollbackEvent},
    local::{self, LibraryScan, LocalBeatmap, LocalBeatmapSet, RepairSeverity, ScanEvent},
    osu_db,
    osu_oauth::{self, OauthSession},
    query::{
        AR_RANGE, BPM_RANGE, BeatmapFilters, CS_RANGE, HP_RANGE, ModeFilter, OD_RANGE, RangeFilter,
        STARS_RANGE,
    },
    shrink::{self, SetShrinkReport, ShrinkEvent, ShrinkJob},
    skin_editor::SkinEditorState,
    updates::{self, OutdatedSet},
};
use anyhow::{Context, Result};
use eframe::egui;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver},
    },
    thread,
    time::{Duration, Instant},
};

const BEATMAPSET_DOWNLOAD_DELAY: Duration = Duration::from_secs(2);
const SCAN_CACHE_VERSION: u32 = 5;
/// Minimum interval between repair-job rebuilds while a scan is streaming.
/// When many maps have missing files, `problems.len()` (part of the rebuild
/// key) grows on every streamed map event, so without this the whole-library
/// rebuild runs nearly every frame for the entire scan.
const REPAIR_JOBS_REBUILD_INTERVAL: Duration = Duration::from_secs(1);
/// Job cards rendered on the Maintenance tab before the list is cut off. When
/// every map is flagged (e.g. deleted backgrounds) there can be thousands of
/// cards and egui lays all of them out on every frame the tab is open; the
/// totals above the list and `Repair all` still cover the full set.
const MAX_RENDERED_REPAIR_JOBS: usize = 200;
/// Raw issue rows rendered before the list is cut off (same rationale).
const MAX_RENDERED_REPAIR_ISSUES: usize = 500;
/// Most recent repair-log entries rendered; older ones stay hidden while the
/// totals and progress bar above cover the whole batch.
const MAX_RENDERED_REPAIR_LOG: usize = 50;
const BACKGROUND_CACHE_LIMIT: usize = 24;
const BACKGROUND_PREVIEW_WIDTH: u16 = 1200;
const BACKGROUND_PREVIEW_HEIGHT: u16 = 675;
/// Fixed on-screen height of the inspector background box. Every background
/// (and the missing-background banner) renders at exactly this size.
const MAP_PREVIEW_HEIGHT: f32 = 270.0;
/// Fixed on-screen height of the Extras tab's imported-image preview box.
const EXTRAS_PREVIEW_HEIGHT: f32 = 320.0;
/// How many neighbors on each side of the selected map get decoded ahead of
/// time so stepping through the list usually hits the cache.
const BACKGROUND_PREFETCH_RADIUS: usize = 3;
/// Upper bound on concurrent background decodes so fast scrolling cannot pile
/// up threads and starve the currently visible preview.
const BACKGROUND_MAX_IN_FLIGHT: usize = 4;
/// Delay between update-check metadata requests so a big library does not
/// hammer the osu! API through the Worker.
const UPDATE_CHECK_DELAY: Duration = Duration::from_millis(300);
/// Slower pacing for unsigned checks: those spend the Worker's shared app
/// quota, which osu! asks clients to keep under 60 req/min (~1 req/s).
/// Signed-in checks spend the user's own quota instead.
const UPDATE_CHECK_DELAY_ANONYMOUS: Duration = Duration::from_millis(1100);

pub struct MapManagerApp {
    active_tab: AppTab,
    filters: BeatmapFilters,
    /// Mode filter as of the previous frame, so switching it can report how
    /// many selected maps no longer match instead of silently dropping them.
    last_mode_filter: ModeFilter,
    songs_dir: String,
    /// Whether the raw Songs-folder path field is shown. Hidden behind a
    /// "Change…" button once a folder is detected so the first screen is a
    /// status, not a filesystem path.
    songs_dir_editing: bool,
    /// osu! root derived from `songs_dir` (its parent when pointing at a
    /// `Songs` folder). Tracks which install the per-library state belongs to.
    loaded_root: String,
    collection_name: String,
    collections: Vec<collection::CollectionEntry>,
    selected_collection_index: Option<usize>,
    collection_missing_hashes: Vec<String>,
    collection_notice: Option<CollectionNotice>,
    oauth_session: Option<OauthSession>,
    oauth_status: String,
    oauth_pending_url: Option<String>,
    is_signing_in: bool,
    selected_maps: Vec<LocalBeatmap>,
    selected_md5s: BTreeSet<String>,
    filtered_map_indexes: Vec<usize>,
    filtered_cache_key: String,
    /// md5 → position inside `filtered_map_indexes`, rebuilt together with
    /// the filtered list so inspector/prefetch lookups stay O(1) instead of
    /// scanning the list every frame.
    filtered_pos_by_map_index: HashMap<usize, usize>,
    /// md5 → position inside the scan's map list, rebuilt lazily when the
    /// scan changes. Avoids a full linear scan on every frame while a map
    /// is selected.
    md5_to_map_index: HashMap<String, usize>,
    md5_index_key: (u64, usize),
    /// Cached `collection_backup_info` result: re-parsing `collection.db`
    /// every frame (sidebar + center + dialogs all ask for it) means disk
    /// I/O plus a full parse at 60 fps. The key is the db path plus the
    /// backup file's size/mtime, so saves and restores invalidate it.
    backup_info_cache_path: Option<PathBuf>,
    backup_info_cache_file: Option<(u64, u64, u32)>,
    backup_info_cached: Option<BackupInfo>,
    /// Cached md5 → map lookup for the selected collection's contents.
    /// Rebuilding it from the whole scan every frame is O(library) per
    /// frame; the key is (selected collection, scan generation).
    collection_contents_key: (Option<usize>, u64),
    collection_contents_cache: BTreeMap<String, LocalBeatmap>,
    /// Per-collection auto-add settings, persisted in a sidecar JSON next to
    /// the scan cache (`collection.db` is osu!'s format and cannot carry
    /// map-manager-specific fields).
    auto_collections: collection::AutoCollectionStore,
    /// md5s seen in the last completed scan — the baseline deciding which
    /// freshly scanned maps count as "newly downloaded" for auto-add.
    /// Deliberately not trimmed by folder pruning, so repair/update rescans
    /// diff against the full previous library.
    last_scan_md5s: HashSet<String>,
    /// Cached "N library maps match" count for the auto-add editor, keyed by
    /// serialized filters + scan generation (same idea as `filtered_cache_key`).
    auto_match_cache_key: (String, u64),
    auto_match_cache_count: Option<usize>,
    /// Bumped on every scan mutation (finish, stop, prune, delete) so the
    /// filtered-list and repair-job caches cannot go stale when the map count
    /// alone does not change.
    scan_generation: u64,
    repair_jobs_cache: Vec<RepairJob>,
    repair_jobs_cache_key: String,
    /// Last time the repair-jobs cache was rebuilt, used to rate-limit
    /// rebuilds while a scan is streaming (see `refresh_repair_jobs`).
    repair_jobs_last_rebuild: Instant,
    scan: Option<LibraryScan>,
    is_scanning: bool,
    is_repairing: bool,
    skip_parse_timeouts: bool,
    skip_parse_errors: bool,
    delete_taiko: bool,
    delete_catch: bool,
    delete_mania: bool,
    scanned_maps: usize,
    scan_total: usize,
    matched_maps: usize,
    star_parse_error: Option<String>,
    repair_progress: String,
    repair_total: usize,
    repair_done: usize,
    repair_successes: usize,
    repair_failures: usize,
    repair_log: Vec<RepairLogEntry>,
    repair_touched_folders: BTreeSet<PathBuf>,
    repair_ignores: RepairIgnoreStore,
    outdated_sets: Vec<OutdatedSet>,
    is_checking_updates: bool,
    update_check_pause: Option<Arc<AtomicBool>>,
    update_check_cancel: Option<Arc<AtomicBool>>,
    update_check_done: usize,
    update_check_total: usize,
    update_check_uncheckable: usize,
    update_unavailable: usize,
    update_skipped: usize,
    /// osu!.db pre-filter context for the latest/current check: why the
    /// filter is unavailable or how many sets it removed. Rendered as its
    /// own persistent label so progress messages cannot flash it away.
    update_check_db_note: Option<String>,
    update_check_status: String,
    is_updating: bool,
    update_pause: Option<Arc<AtomicBool>>,
    update_cancel: Option<Arc<AtomicBool>>,
    update_progress: String,
    update_total: usize,
    update_done: usize,
    update_successes: usize,
    update_failures: usize,
    update_log: Vec<RepairLogEntry>,
    update_touched_folders: BTreeSet<PathBuf>,
    // ── App self-update (passive startup highlight + manual button) ──
    app_update_rx: Option<Receiver<AppUpdateEvent>>,
    app_update_window_open: bool,
    app_update_checking: bool,
    /// `true` while the one-shot startup check runs; its result never
    /// touches the status bar, it only drives the button highlight.
    app_update_background_check: bool,
    app_update_status: String,
    app_update_release: Option<AppRelease>,
    app_update_up_to_date: bool,
    app_update_downloading: bool,
    app_update_downloaded: u64,
    app_update_total: Option<u64>,
    app_update_installing: bool,
    app_update_error: Option<String>,
    expanded_map_md5: Option<String>,
    delete_confirmation: Option<DeleteIntent>,
    background_preview_path: Option<PathBuf>,
    background_preview: Option<egui::TextureHandle>,
    background_preview_error: Option<String>,
    background_load_tx: mpsc::Sender<(PathBuf, Result<egui::ColorImage>)>,
    background_load_rx: mpsc::Receiver<(PathBuf, Result<egui::ColorImage>)>,
    background_in_flight: HashSet<PathBuf>,
    background_cache: HashMap<PathBuf, egui::TextureHandle>,
    background_cache_order: Vec<PathBuf>,
    audio_backend: Option<AudioBackend>,
    audio_player: Option<AudioPlayer>,
    audio_volume: f32,
    status: String,
    scan_rx: Option<Receiver<ScanEvent>>,
    scan_cancel: Option<Arc<AtomicBool>>,
    repair_rx: Option<Receiver<RepairEvent>>,
    oauth_rx: Option<Receiver<Result<OauthSession>>>,
    update_check_rx: Option<Receiver<UpdateCheckEvent>>,
    update_rx: Option<Receiver<UpdateEvent>>,
    // ── Shrink tab ──
    shrink_reports: Vec<SetShrinkReport>,
    shrink_options: shrink::ShrinkOptions,
    /// ffprobe results reused across analyses (path + size + mtime key).
    shrink_probe_cache: shrink::ProbeCache,
    is_analyzing: bool,
    analysis_done: usize,
    analysis_total: usize,
    analysis_rx: Option<Receiver<ShrinkAnalysisEvent>>,
    analysis_cancel: Option<Arc<AtomicBool>>,
    analysis_pause: Option<Arc<AtomicBool>>,
    is_shrinking: bool,
    shrink_progress: String,
    shrink_total_assets: usize,
    shrink_done_assets: usize,
    shrink_saved_bytes: u64,
    shrink_successes: usize,
    shrink_failures: usize,
    shrink_log: Vec<JobLogEntry>,
    shrink_rx: Option<Receiver<ShrinkEvent>>,
    shrink_cancel: Option<Arc<AtomicBool>>,
    shrink_pause: Option<Arc<AtomicBool>>,
    shrink_touched_folders: BTreeSet<PathBuf>,
    shrink_backups: Vec<ShrinkBackupRecord>,
    // ── Extras tab ──
    /// Imported image ready to apply: validated, decoded for the preview and
    /// kept as raw bytes so every folder gets a byte-identical copy.
    extras_image_bytes: Option<Arc<Vec<u8>>>,
    /// Lowercase content format of the imported image (jpg/png/webp/bmp).
    extras_image_ext: Option<String>,
    extras_image_name: Option<String>,
    extras_image_texture: Option<egui::TextureHandle>,
    extras_image_size: Option<(u32, u32)>,
    extras_image_rx: Option<Receiver<Result<ExtrasImage>>>,
    extras_running: bool,
    extras_progress: String,
    extras_folders_done: usize,
    extras_folders_total: usize,
    extras_files_replaced: usize,
    extras_files_cached: usize,
    extras_files_skipped: usize,
    extras_successes: usize,
    extras_failures: usize,
    extras_log: Vec<JobLogEntry>,
    extras_rx: Option<Receiver<ExtrasEvent>>,
    extras_cancel: Option<Arc<AtomicBool>>,
    extras_touched_folders: BTreeSet<PathBuf>,
    // ── Extras rollback ──
    /// Summary of the newest apply job's manifest on disk; the rollback
    /// card offers to undo exactly this job.
    extras_last_job: Option<LastExtrasJob>,
    extras_last_job_loaded: bool,
    extras_rolling_back: bool,
    rollback_progress: String,
    rollback_folders_done: usize,
    rollback_folders_total: usize,
    rollback_restored: usize,
    rollback_successes: usize,
    rollback_failures: usize,
    rollback_rx: Option<Receiver<RollbackEvent>>,
    rollback_cancel: Option<Arc<AtomicBool>>,
    rollback_touched_folders: BTreeSet<PathBuf>,
    skin_editor: SkinEditorState,
}

#[derive(Debug, Clone)]
enum DeleteIntent {
    Collection(String),
    NonStdModes(usize),
    RestoreBackup,
    SaveCollection {
        name: String,
        maps: usize,
        hashes: usize,
        exists: bool,
    },
    RenameCollection {
        old: String,
        new: String,
    },
    /// Bulk background replacement asks for explicit confirmation because it
    /// overwrites the background image files across the whole library.
    SetBackground {
        sets: usize,
    },
    /// Shrink overwrites the audio/video/image contents of every analyzed
    /// set, so it confirms just like the other whole-library writes.
    ShrinkRun {
        sets: usize,
    },
    RestoringShrinkBackup {
        index: usize,
        folder: String,
    },
    ExtrasRollback {
        folders: usize,
        image_name: String,
    },
}

/// Inline result of the last collection action, shown on the Collections
/// page itself instead of only in the bottom status bar.
#[derive(Clone)]
struct CollectionNotice {
    ok: bool,
    message: String,
}

/// What the automatic backup holds: when it was written and how many
/// collections/maps it contains. Shown next to the undo button and inside
/// its confirmation so the restore point is obvious.
#[derive(Clone)]
struct BackupInfo {
    when: String,
    collections: usize,
    maps: usize,
}

fn collection_backup_info(db_path: Option<PathBuf>) -> Option<BackupInfo> {
    let backup = db_path?.with_extension("db.bak");
    let modified = fs::metadata(&backup).ok()?.modified().ok()?;
    let when = chrono::DateTime::<chrono::Local>::from(modified)
        .format("%Y-%m-%d %H:%M")
        .to_string();
    let (collections, maps) = collection::load_collection_db(&backup)
        .map(|db| {
            (
                db.collections.len(),
                db.collections.iter().map(|c| c.hashes.len()).sum(),
            )
        })
        .unwrap_or((0, 0));
    Some(BackupInfo {
        when,
        collections,
        maps,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppTab {
    Library,
    Collections,
    Maintenance,
    Shrink,
    SkinEditor,
    Extras,
}

impl AppTab {
    fn label(self) -> &'static str {
        match self {
            Self::Library => "Library",
            Self::Collections => "Collections",
            Self::Maintenance => "Fix maps",
            Self::Shrink => "Shrink",
            Self::SkinEditor => "Skin Editor",
            Self::Extras => "Backgrounds",
        }
    }

    fn subtitle(self) -> &'static str {
        match self {
            Self::Library => "Scan, filter and pick maps",
            Self::Collections => "Build and save collections",
            Self::Maintenance => "Repair, update and clean up",
            Self::Shrink => "Compress audio, video and backgrounds",
            Self::SkinEditor => "Preview, remix and save skins",
            Self::Extras => "Set one background for every map",
        }
    }
}

/// Long-lived audio output device, opened once and shared by every preview.
/// Reopening the default device per track is slow and can fail on devices
/// that allow only one open handle.
struct AudioBackend {
    _stream: rodio::OutputStream,
    handle: rodio::OutputStreamHandle,
}

struct AudioPlayer {
    sink: rodio::Sink,
    path: PathBuf,
}

struct FfmpegPcmSource {
    child: std::process::Child,
    stdout: io::BufReader<std::process::ChildStdout>,
}

impl AudioPlayer {
    fn start(stream_handle: &rodio::OutputStreamHandle, path: &Path, volume: f32) -> Result<Self> {
        let sink = rodio::Sink::try_new(stream_handle).context("creating the audio player")?;
        let source = FfmpegPcmSource::spawn(path)?;
        sink.set_volume(volume);
        sink.append(source);
        sink.play();
        Ok(Self {
            sink,
            path: path.to_owned(),
        })
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        self.sink.stop();
    }
}

impl FfmpegPcmSource {
    const CHANNELS: u16 = 2;
    const SAMPLE_RATE: u32 = 44_100;

    fn spawn(path: &Path) -> Result<Self> {
        let mut command = std::process::Command::new(ffmpeg_executable());
        command
            .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-i"])
            .arg(path)
            .args([
                "-vn",
                "-f",
                "s16le",
                "-acodec",
                "pcm_s16le",
                "-ac",
                "2",
                "-ar",
                "44100",
                "pipe:1",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());

        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }

        let mut child = command.spawn().with_context(
            || "starting FFmpeg; install FFmpeg or place ffmpeg.exe beside the app",
        )?;
        let stdout = child
            .stdout
            .take()
            .context("capturing decoded audio from FFmpeg")?;
        Ok(Self {
            child,
            stdout: io::BufReader::new(stdout),
        })
    }
}

impl Iterator for FfmpegPcmSource {
    type Item = i16;

    fn next(&mut self) -> Option<Self::Item> {
        let mut bytes = [0_u8; 2];
        std::io::Read::read_exact(&mut self.stdout, &mut bytes)
            .ok()
            .map(|()| i16::from_le_bytes(bytes))
    }
}

impl rodio::Source for FfmpegPcmSource {
    fn current_frame_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> u16 {
        Self::CHANNELS
    }

    fn sample_rate(&self) -> u32 {
        Self::SAMPLE_RATE
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

impl Drop for FfmpegPcmSource {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Debug)]
enum AppUpdateEvent {
    CheckResult(Result<Option<AppRelease>, String>),
    DownloadProgress { done: u64, total: Option<u64> },
    DownloadFinished,
    Failed { message: String },
}

/// Shared by the startup highlight check and the manual "Check now" button.
fn spawn_app_update_check_worker(tx: mpsc::Sender<AppUpdateEvent>) {
    std::thread::spawn(move || {
        let current = app_update::current_version_text();
        let result = match app_update::fetch_latest_release() {
            Ok(None) => Ok(None),
            Ok(Some(release)) if !app_update::is_newer_version(&release.tag, &current) => Ok(None),
            Ok(Some(release)) => Ok(Some(release)),
            Err(err) => Err(format!("{err:#}")),
        };
        let _ = tx.send(AppUpdateEvent::CheckResult(result));
    });
}

#[derive(Debug)]
enum RepairEvent {
    Started {
        total: usize,
    },
    Opening {
        beatmapset_id: i64,
        index: usize,
        total: usize,
    },
    Repaired {
        beatmapset_id: i64,
        folders: Vec<PathBuf>,
        restored_files: Vec<String>,
        download_source: String,
        ignored_after_success: Vec<IgnoredRepairIssue>,
    },
    Failed {
        beatmapset_id: i64,
        message: String,
    },
    Finished,
}

#[derive(Debug)]
enum UpdateCheckEvent {
    Started {
        total: usize,
        uncheckable: usize,
        /// Sets removed by the osu!.db pre-filter before any network
        /// request; counted once instead of logged per set.
        db_skipped: usize,
        /// Context about the osu!.db pre-filter: either why it is
        /// unavailable (every set will be checked online) or how many sets
        /// it removed.
        db_note: Option<String>,
    },
    Checked {
        done: usize,
        total: usize,
    },
    Found(OutdatedSet),
    Skipped {
        beatmapset_id: i64,
        reason: String,
    },
    Unavailable {
        beatmapset_id: Option<i64>,
        reason: String,
    },
    Finished,
}

#[derive(Debug)]
enum UpdateEvent {
    Started {
        total: usize,
    },
    Opening {
        beatmapset_id: i64,
        index: usize,
        total: usize,
    },
    Updated {
        beatmapset_id: i64,
        folders: Vec<PathBuf>,
        written: usize,
        removed: usize,
        download_source: String,
        checksum_note: Option<String>,
    },
    Failed {
        beatmapset_id: i64,
        message: String,
    },
    Finished,
}

#[derive(Debug, Clone)]
struct RepairLogEntry {
    beatmapset_id: i64,
    status: RepairLogStatus,
    message: String,
}

#[derive(Debug)]
enum ShrinkAnalysisEvent {
    Started { sets: usize },
    Report { report: SetShrinkReport },
    Finished { cache: shrink::ProbeCache },
}

#[derive(Debug, Clone)]
/// One row in a batch job's activity log. Shared by the Shrink and Extras
/// tabs, whose logs carry the same label/status/message shape.
struct JobLogEntry {
    label: String,
    status: RepairLogStatus,
    message: String,
}

/// An image picked for the Extras background job: decoded once on a worker
/// thread (which also validates that osu! can load it), then kept as raw
/// bytes for the per-folder copies plus a downscaled preview for the UI.
struct ExtrasImage {
    bytes: Arc<Vec<u8>>,
    ext: String,
    preview: egui::ColorImage,
    size: (u32, u32),
}

/// What the newest rollback manifest on disk says, enough for the Extras
/// rollback card. The full manifest is read by the rollback worker only.
#[derive(Debug, Clone)]
struct LastExtrasJob {
    manifest_path: PathBuf,
    when: String,
    image_name: String,
    folders: usize,
}

/// One session backup: the zip plus the set folder it restores.
#[derive(Debug, Clone)]
struct ShrinkBackupRecord {
    zip: PathBuf,
    folder: PathBuf,
}

#[derive(Debug, Clone, Copy)]
enum RepairLogStatus {
    InProgress,
    Success,
    Failed,
    /// Nothing was wrong — the item did not need the action (e.g. ranked,
    /// qualified or loved beatmapsets, which the update check skips).
    Skipped,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RepairIgnoreStore {
    entries: Vec<IgnoredRepairIssue>,
    /// When set, missing-background issues are hidden from counts, lists and
    /// repair jobs entirely — for libraries whose backgrounds were deleted
    /// deliberately to save space. Persisted so the choice survives restarts.
    #[serde(default)]
    ignore_missing_backgrounds: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ScanCacheFile {
    version: u32,
    scan: LibraryScan,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct IgnoredRepairIssue {
    beatmapset_id: Option<i64>,
    beatmap_id: Option<i64>,
    beatmap_file: String,
    message: String,
}

impl RepairIgnoreStore {
    fn ignores(&self, map: &LocalBeatmap, issue: &local::RepairIssue) -> bool {
        if issue.severity != RepairSeverity::MissingRequiredFile {
            return false;
        }

        let file = map
            .path
            .file_name()
            .and_then(|file| file.to_str())
            .unwrap_or_default();

        self.entries.iter().any(|entry| {
            entry.beatmapset_id == map.beatmapset_id
                && entry.beatmap_id == map.beatmap_id
                && entry.beatmap_file == file
                && entry.message == issue.message
        })
    }
}

impl MapManagerApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        apply_theme(&cc.egui_ctx);

        let osu_root = default_osu_root()
            .map(|path| display_prefilled_path(&path))
            .unwrap_or_default();
        let songs_dir = if osu_root.is_empty() {
            String::new()
        } else {
            format!("{osu_root}\\Songs")
        };
        let loaded_root = derive_osu_root(&songs_dir);
        let repair_ignores = load_repair_ignores(&osu_root).unwrap_or_default();
        let oauth_session = osu_oauth::load_oauth_session(&osu_root);
        let oauth_status = if oauth_session.is_some() {
            "Signed in with osu! (token loaded from disk)".to_owned()
        } else {
            "Not signed in with osu!".to_owned()
        };
        let filters = BeatmapFilters::with_full_ranges();
        let cached_scan = load_scan_cache(&osu_root).ok().flatten();
        let last_scan_md5s = scan_md5_baseline(cached_scan.as_ref());
        let auto_collections =
            collection::AutoCollectionStore::load(&auto_collections_path(&osu_root));
        let cached_status = cached_scan.as_ref().map(|scan| {
            let matching_maps = scan
                .maps
                .iter()
                .filter(|map| matches_visible_filters(&filters, map))
                .count();
            let repair_issues = visible_problems(
                &scan.problems,
                repair_ignores.ignore_missing_backgrounds,
            )
            .count();
            format!(
                "Loaded cached scan: {matching_maps} matching maps from {} scanned maps, {} sets, {}",
                scan.maps.len(),
                scan.sets.len(),
                plural(repair_issues, "repair issue", "repair issues")
            )
        });

        let (background_load_tx, background_load_rx) = mpsc::channel();

        let mut app = Self {
            active_tab: AppTab::Library,
            filters,
            last_mode_filter: ModeFilter::default(),
            songs_dir_editing: songs_dir.is_empty(),
            songs_dir,
            loaded_root,
            collection_name: "osu-map-manager".to_owned(),
            collections: Vec::new(),
            selected_collection_index: None,
            collection_missing_hashes: Vec::new(),
            collection_notice: None,
            oauth_session,
            oauth_status,
            oauth_pending_url: None,
            is_signing_in: false,
            selected_maps: Vec::new(),
            selected_md5s: BTreeSet::new(),
            filtered_map_indexes: Vec::new(),
            filtered_cache_key: String::new(),
            filtered_pos_by_map_index: HashMap::new(),
            md5_to_map_index: HashMap::new(),
            md5_index_key: (u64::MAX, usize::MAX),
            backup_info_cache_path: None,
            backup_info_cache_file: None,
            backup_info_cached: None,
            collection_contents_key: (None, u64::MAX),
            collection_contents_cache: BTreeMap::new(),
            auto_collections,
            last_scan_md5s,
            auto_match_cache_key: (String::new(), 0),
            auto_match_cache_count: None,
            scan_generation: 0,
            repair_jobs_cache: Vec::new(),
            repair_jobs_cache_key: String::new(),
            repair_jobs_last_rebuild: Instant::now(),
            scan: cached_scan,
            is_scanning: false,
            is_repairing: false,
            skip_parse_timeouts: false,
            skip_parse_errors: false,
            delete_taiko: true,
            delete_catch: true,
            delete_mania: true,
            scanned_maps: 0,
            scan_total: 0,
            matched_maps: 0,
            star_parse_error: None,
            repair_progress: String::new(),
            repair_total: 0,
            repair_done: 0,
            repair_successes: 0,
            repair_failures: 0,
            repair_log: Vec::new(),
            repair_touched_folders: BTreeSet::new(),
            repair_ignores,
            outdated_sets: Vec::new(),
            is_checking_updates: false,
            update_check_pause: None,
            update_check_cancel: None,
            update_check_done: 0,
            update_check_total: 0,
            update_check_uncheckable: 0,
            update_unavailable: 0,
            update_skipped: 0,
            update_check_db_note: None,
            update_check_status: String::new(),
            is_updating: false,
            update_pause: None,
            update_cancel: None,
            update_progress: String::new(),
            update_total: 0,
            update_done: 0,
            update_successes: 0,
            update_failures: 0,
            update_log: Vec::new(),
            update_touched_folders: BTreeSet::new(),
            expanded_map_md5: None,
            delete_confirmation: None,
            background_preview_path: None,
            background_preview: None,
            background_preview_error: None,
            background_load_tx,
            background_load_rx,
            background_in_flight: HashSet::new(),
            background_cache: HashMap::new(),
            background_cache_order: Vec::new(),
            audio_backend: None,
            audio_player: None,
            audio_volume: 0.8,
            status: cached_status.unwrap_or_else(|| "Ready".to_owned()),
            scan_rx: None,
            scan_cancel: None,
            repair_rx: None,
            oauth_rx: None,
            update_check_rx: None,
            update_rx: None,
            app_update_rx: None,
            app_update_window_open: false,
            app_update_checking: false,
            app_update_background_check: false,
            app_update_status: String::new(),
            app_update_release: None,
            app_update_up_to_date: false,
            app_update_downloading: false,
            app_update_downloaded: 0,
            app_update_total: None,
            app_update_installing: false,
            app_update_error: None,
            shrink_reports: Vec::new(),
            shrink_options: shrink::ShrinkOptions::default(),
            shrink_probe_cache: HashMap::new(),
            is_analyzing: false,
            analysis_done: 0,
            analysis_total: 0,
            analysis_rx: None,
            analysis_cancel: None,
            analysis_pause: None,
            is_shrinking: false,
            shrink_progress: String::new(),
            shrink_total_assets: 0,
            shrink_done_assets: 0,
            shrink_saved_bytes: 0,
            shrink_successes: 0,
            shrink_failures: 0,
            shrink_log: Vec::new(),
            shrink_rx: None,
            shrink_cancel: None,
            shrink_pause: None,
            shrink_touched_folders: BTreeSet::new(),
            shrink_backups: Vec::new(),
            extras_image_bytes: None,
            extras_image_ext: None,
            extras_image_name: None,
            extras_image_texture: None,
            extras_image_size: None,
            extras_image_rx: None,
            extras_running: false,
            extras_progress: String::new(),
            extras_folders_done: 0,
            extras_folders_total: 0,
            extras_files_replaced: 0,
            extras_files_cached: 0,
            extras_files_skipped: 0,
            extras_successes: 0,
            extras_failures: 0,
            extras_log: Vec::new(),
            extras_rx: None,
            extras_cancel: None,
            extras_touched_folders: BTreeSet::new(),
            extras_last_job: None,
            extras_last_job_loaded: false,
            extras_rolling_back: false,
            rollback_progress: String::new(),
            rollback_folders_done: 0,
            rollback_folders_total: 0,
            rollback_restored: 0,
            rollback_successes: 0,
            rollback_failures: 0,
            rollback_rx: None,
            rollback_cancel: None,
            rollback_touched_folders: BTreeSet::new(),
            skin_editor: SkinEditorState::new(),
        };
        app.load_collections();
        app.start_app_update_background_check();
        app
    }

    fn poll_background(&mut self, ctx: &egui::Context) {
        self.poll_background_load(ctx);
        self.poll_extras(ctx);
        self.poll_extras_rollback();
        self.poll_app_update();
        if let Some(rx) = self.scan_rx.take() {
            let mut keep_rx = true;
            let mut disconnected = false;
            loop {
                let event = match rx.try_recv() {
                    Ok(event) => event,
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                };
                match event {
                    ScanEvent::Started {
                        total_osu_files,
                        star_parse_error,
                    } => {
                        self.scan = Some(LibraryScan::default());
                        self.expanded_map_md5 = None;
                        self.clear_background_preview();
                        self.selected_maps.clear();
                        self.selected_md5s.clear();
                        self.invalidate_scan_caches();
                        self.is_scanning = true;
                        self.scanned_maps = 0;
                        self.scan_total = total_osu_files;
                        self.matched_maps = 0;
                        self.star_parse_error = star_parse_error;
                        self.status = scan_progress_status(
                            self.scanned_maps,
                            self.scan_total,
                            self.matched_maps,
                        );
                    }
                    ScanEvent::Map {
                        map,
                        file_len,
                        file_mtime_secs,
                        issues,
                    } => {
                        if self.scan_cancel.is_none() {
                            continue;
                        }
                        self.scanned_maps += 1;
                        if matches_visible_filters(&self.filters, &map) {
                            self.matched_maps += 1;
                        }
                        let visible_issues = issues
                            .into_iter()
                            .filter(|issue| !self.repair_ignores.ignores(&map, issue))
                            .collect::<Vec<_>>();
                        if let Some(scan) = &mut self.scan {
                            scan.file_meta
                                .insert(map.path.clone(), (file_len, file_mtime_secs));
                            scan.problems.extend(visible_issues);
                            scan.maps.push(*map);
                        }
                        self.status = scan_progress_status(
                            self.scanned_maps,
                            self.scan_total,
                            self.matched_maps,
                        );
                    }
                    ScanEvent::Problem { issue } => {
                        if let Some(scan) = &mut self.scan {
                            scan.problems.push(issue);
                        }
                    }
                    ScanEvent::Finished { sets } => {
                        self.is_scanning = false;
                        self.scan_cancel = None;
                        self.scan_generation += 1;
                        let ignore_missing_backgrounds =
                            self.repair_ignores.ignore_missing_backgrounds;
                        let cache_root = self.osu_root();
                        if let Some(scan) = &mut self.scan {
                            scan.sets = sets;
                            let matching_maps = scan
                                .maps
                                .iter()
                                .filter(|map| matches_visible_filters(&self.filters, map))
                                .count();
                            let repair_issues =
                                visible_problems(&scan.problems, ignore_missing_backgrounds)
                                    .count();
                            self.status = format!(
                                "Scan complete: {} matching maps from {} scanned maps, {} sets, {}",
                                matching_maps,
                                scan.maps.len(),
                                scan.sets.len(),
                                plural(repair_issues, "repair issue", "repair issues")
                            );
                            if let Err(err) = save_scan_cache(&cache_root, scan) {
                                self.status =
                                    format!("{}; cache save failed: {err:#}", self.status);
                            }
                        } else {
                            self.status = "Scan complete".to_owned();
                        }
                        // Auto-add freshly discovered maps to every enabled
                        // collection, then refresh the "seen" baseline so the
                        // next scan diffs against this one.
                        if self
                            .auto_collections
                            .collections
                            .values()
                            .any(|config| config.enabled)
                        {
                            let new_maps = self
                                .scan
                                .as_ref()
                                .map(|scan| {
                                    scan.maps
                                        .iter()
                                        .filter(|map| !map.md5.is_empty())
                                        .filter(|map| !self.last_scan_md5s.contains(&map.md5))
                                        .cloned()
                                        .collect::<Vec<_>>()
                                })
                                .unwrap_or_default();
                            self.auto_add_new_maps(&new_maps);
                        }
                        self.last_scan_md5s = scan_md5_baseline(self.scan.as_ref());
                        keep_rx = false;
                    }
                    ScanEvent::Stopped { sets } => {
                        self.is_scanning = false;
                        self.scan_cancel = None;
                        self.scan_generation += 1;
                        let ignore_missing_backgrounds =
                            self.repair_ignores.ignore_missing_backgrounds;
                        if let Some(scan) = &mut self.scan {
                            scan.sets = sets;
                            let matching_maps = scan
                                .maps
                                .iter()
                                .filter(|map| matches_visible_filters(&self.filters, map))
                                .count();
                            let repair_issues =
                                visible_problems(&scan.problems, ignore_missing_backgrounds)
                                    .count();
                            self.status = format!(
                                "Scan stopped: {} matching maps from {} scanned maps, {} sets, {}",
                                matching_maps,
                                scan.maps.len(),
                                scan.sets.len(),
                                plural(repair_issues, "repair issue", "repair issues")
                            );
                        } else {
                            self.status = "Scan stopped".to_owned();
                        }
                        keep_rx = false;
                    }
                    ScanEvent::Failed { message } => {
                        self.is_scanning = false;
                        self.scan_cancel = None;
                        self.status = format!("Scan failed: {message}");
                        keep_rx = false;
                    }
                }
            }

            if keep_rx {
                if disconnected {
                    self.is_scanning = false;
                    self.scan_cancel = None;
                    self.status = "Scan worker disconnected".to_owned();
                } else {
                    self.scan_rx = Some(rx);
                }
            }
        }

        if let Some(rx) = self.repair_rx.take() {
            let mut keep_rx = true;
            let mut disconnected = false;
            loop {
                let event = match rx.try_recv() {
                    Ok(event) => event,
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                };
                match event {
                    RepairEvent::Started { total } => {
                        self.is_repairing = true;
                        self.repair_total = total;
                        self.repair_done = 0;
                        self.repair_successes = 0;
                        self.repair_failures = 0;
                        self.repair_log.clear();
                        self.repair_touched_folders.clear();
                        self.repair_progress = format!(
                            "Preparing to repair {}",
                            plural(total, "beatmapset", "beatmapsets")
                        );
                        self.status = self.repair_progress.clone();
                    }
                    RepairEvent::Opening {
                        beatmapset_id,
                        index,
                        total,
                    } => {
                        self.upsert_repair_log(
                            beatmapset_id,
                            RepairLogStatus::InProgress,
                            format!("Downloading {index}/{total}"),
                        );
                        self.repair_progress =
                            format!("Downloading repair {index}/{total}: set {beatmapset_id}");
                        self.status = self.repair_progress.clone();
                    }
                    RepairEvent::Repaired {
                        beatmapset_id,
                        folders,
                        restored_files,
                        download_source,
                        ignored_after_success,
                    } => {
                        self.repair_done += 1;
                        self.repair_successes += 1;
                        self.repair_touched_folders.extend(folders.clone());
                        let folder_count = folders.len();
                        let ignored_count = self.add_repair_ignores(ignored_after_success);
                        let restored = if restored_files.is_empty() {
                            "no missing files remained".to_owned()
                        } else {
                            format!("restored {}", restored_files.join(", "))
                        };
                        self.upsert_repair_log(
                            beatmapset_id,
                            RepairLogStatus::Success,
                            format!(
                                "Repaired {} via {download_source}; {restored}; ignored {}",
                                plural(folder_count, "folder", "folders"),
                                plural(
                                    ignored_count,
                                    "missing background issue",
                                    "missing background issues"
                                )
                            ),
                        );
                        self.repair_progress = format!(
                            "Repaired set {beatmapset_id} in {}",
                            plural(folder_count, "folder", "folders")
                        );
                        self.status = self.repair_progress.clone();
                    }
                    RepairEvent::Failed {
                        beatmapset_id,
                        message,
                    } => {
                        self.repair_done += 1;
                        self.repair_failures += 1;
                        self.upsert_repair_log(
                            beatmapset_id,
                            RepairLogStatus::Failed,
                            message.clone(),
                        );
                        self.repair_progress =
                            format!("Repair failed for set {beatmapset_id}: {message}");
                        self.status = self.repair_progress.clone();
                    }
                    RepairEvent::Finished => {
                        self.is_repairing = false;
                        self.status = format!(
                            "Repair finished: {} succeeded, {} failed out of {}",
                            self.repair_successes, self.repair_failures, self.repair_total
                        );
                        let touched = std::mem::take(&mut self.repair_touched_folders);
                        if !touched.is_empty() {
                            self.prune_scan_folders(&touched);
                            self.status.push_str("; rescanning repaired files");
                        }
                        keep_rx = false;
                        if !touched.is_empty() && !self.is_scanning {
                            self.start_scan();
                        }
                    }
                }
            }

            if keep_rx {
                if disconnected {
                    self.is_repairing = false;
                    self.status = "Repair worker disconnected".to_owned();
                } else {
                    self.repair_rx = Some(rx);
                }
            }
        }

        if let Some(rx) = self.analysis_rx.take() {
            let mut keep_rx = true;
            let mut disconnected = false;
            loop {
                let event = match rx.try_recv() {
                    Ok(event) => event,
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                };
                match event {
                    ShrinkAnalysisEvent::Started { sets } => {
                        self.analysis_total = sets;
                        self.analysis_done = 0;
                        self.status =
                            format!("Checking {}…", plural(sets, "set folder", "set folders"));
                    }
                    ShrinkAnalysisEvent::Report { report } => {
                        self.analysis_done += 1;
                        self.shrink_reports.push(report);
                        self.status = format!(
                            "Checked {}/{}…",
                            self.analysis_done,
                            plural(self.analysis_total, "set folder", "set folders")
                        );
                    }
                    ShrinkAnalysisEvent::Finished { cache } => {
                        self.is_analyzing = false;
                        self.shrink_probe_cache = cache;
                        let stopped = self
                            .analysis_cancel
                            .as_ref()
                            .is_some_and(|flag| flag.load(Ordering::Relaxed));
                        // Biggest savings first.
                        self.shrink_reports
                            .sort_by_key(|r| std::cmp::Reverse(r.est_saved()));
                        let (total_in, total_est, items) = shrink::summarize(&self.shrink_reports);
                        let saved = total_in.saturating_sub(total_est);
                        self.status = format!(
                            "Analysis {}: {}, {} shrinkable, est. {} → {} (saves ~{})",
                            if stopped {
                                "stopped (partial results)"
                            } else {
                                "done"
                            },
                            plural(self.shrink_reports.len(), "set", "sets"),
                            plural(items, "file", "files"),
                            shrink::human_bytes(total_in),
                            shrink::human_bytes(total_est),
                            shrink::human_bytes(saved),
                        );
                        keep_rx = false;
                    }
                }
            }
            if keep_rx {
                if disconnected {
                    self.is_analyzing = false;
                    self.status = "Analysis worker disconnected".to_owned();
                } else {
                    self.analysis_rx = Some(rx);
                }
            }
        }

        if let Some(rx) = self.shrink_rx.take() {
            let mut keep_rx = true;
            let mut disconnected = false;
            loop {
                let event = match rx.try_recv() {
                    Ok(event) => event,
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                };
                match event {
                    ShrinkEvent::Started { sets, assets } => {
                        self.is_shrinking = true;
                        self.shrink_total_assets = assets;
                        self.shrink_done_assets = 0;
                        self.shrink_saved_bytes = 0;
                        self.shrink_successes = 0;
                        self.shrink_failures = 0;
                        self.shrink_log.clear();
                        self.shrink_touched_folders.clear();
                        self.shrink_progress = format!(
                            "Shrinking {} in {}",
                            plural(assets, "file", "files"),
                            plural(sets, "set", "sets")
                        );
                        self.status = self.shrink_progress.clone();
                    }
                    ShrinkEvent::SetStarted { label } => {
                        self.shrink_progress = format!(
                            "Shrinking {}/{} files… {label}",
                            self.shrink_done_assets, self.shrink_total_assets
                        );
                        self.status = self.shrink_progress.clone();
                    }
                    ShrinkEvent::AssetDone { saved } => {
                        self.shrink_done_assets += 1;
                        self.shrink_saved_bytes += saved;
                    }
                    ShrinkEvent::SetDone {
                        folder,
                        saved,
                        backup,
                    } => {
                        self.shrink_successes += 1;
                        self.shrink_touched_folders.insert(folder.clone());
                        let label = folder
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("set")
                            .to_owned();
                        if let Some(zip) = backup {
                            self.shrink_backups.push(ShrinkBackupRecord { zip, folder });
                        }
                        self.upsert_shrink_log(
                            label,
                            RepairLogStatus::Success,
                            format!("saved {}", shrink::human_bytes(saved)),
                        );
                    }
                    ShrinkEvent::SetFailed { folder, message } => {
                        self.shrink_failures += 1;
                        self.shrink_touched_folders.insert(folder.clone());
                        let label = folder
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("set")
                            .to_owned();
                        self.upsert_shrink_log(label, RepairLogStatus::Failed, message.clone());
                        self.shrink_progress = format!("Failed: {message}");
                        self.status = self.shrink_progress.clone();
                    }
                    ShrinkEvent::Finished { saved, elapsed_s } => {
                        self.is_shrinking = false;
                        let cancelled = self
                            .shrink_cancel
                            .as_ref()
                            .is_some_and(|flag| flag.load(Ordering::Relaxed));
                        self.status = format!(
                            "Shrink {} in {:.0}s: {} ok, {} failed, saved {} — rescanning",
                            if cancelled { "cancelled" } else { "finished" },
                            elapsed_s,
                            plural(self.shrink_successes, "set", "sets"),
                            plural(self.shrink_failures, "set", "sets"),
                            shrink::human_bytes(saved),
                        );
                        let touched = std::mem::take(&mut self.shrink_touched_folders);
                        if !touched.is_empty() {
                            self.prune_scan_folders(&touched);
                        }
                        // Reports are stale now (assets changed size).
                        self.shrink_reports.clear();
                        keep_rx = false;
                        if !touched.is_empty() && !self.is_scanning {
                            self.start_scan();
                        }
                    }
                }
            }
            if keep_rx {
                if disconnected {
                    self.is_shrinking = false;
                    self.status = "Shrink worker disconnected".to_owned();
                } else {
                    self.shrink_rx = Some(rx);
                }
            }
        }

        if let Some(rx) = self.oauth_rx.take() {
            match rx.try_recv() {
                Ok(result) => {
                    self.is_signing_in = false;
                    self.oauth_pending_url = None;
                    match result {
                        Ok(session) => {
                            self.oauth_session = Some(session.clone());
                            if let Err(err) =
                                osu_oauth::save_oauth_session(&self.osu_root(), &session)
                            {
                                self.oauth_status =
                                    format!("Signed in, but saving the session failed: {err:#}");
                            } else {
                                self.oauth_status =
                                    "Signed in with osu! (downloads use the official osu! API)"
                                        .to_owned();
                            }
                            self.status = self.oauth_status.clone();
                        }
                        Err(err) => {
                            self.oauth_status = format!("osu! sign-in failed: {err:#}");
                            self.status = self.oauth_status.clone();
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {
                    self.oauth_rx = Some(rx);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.is_signing_in = false;
                    self.oauth_status = "osu! sign-in was cancelled".to_owned();
                }
            }
        }

        if let Some(rx) = self.update_check_rx.take() {
            let mut keep_rx = true;
            let mut disconnected = false;
            loop {
                let event = match rx.try_recv() {
                    Ok(event) => event,
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                };
                match event {
                    UpdateCheckEvent::Started {
                        total,
                        uncheckable,
                        db_skipped,
                        db_note,
                    } => {
                        self.is_checking_updates = true;
                        self.update_check_done = 0;
                        self.update_check_total = total;
                        self.update_check_uncheckable = uncheckable;
                        self.update_unavailable = 0;
                        self.update_skipped = db_skipped;
                        self.update_check_db_note = db_note;
                        self.outdated_sets.clear();
                        if db_skipped > 0 {
                            self.update_log.push(RepairLogEntry {
                                beatmapset_id: 0,
                                status: RepairLogStatus::Skipped,
                                message: format!(
                                    "{} ranked/approved/qualified/loved sets \
                                     skipped locally via osu!.db",
                                    db_skipped
                                ),
                            });
                        }
                        self.update_check_status = format!(
                            "Checking {} against osu!web",
                            plural(total, "beatmapset", "beatmapsets")
                        );
                        self.status = self.update_check_status.clone();
                    }
                    UpdateCheckEvent::Checked { done, total } => {
                        self.update_check_done = done;
                        self.update_check_status = format!(
                            "Checked {done}/{total}, {} outdated",
                            self.outdated_sets.len()
                        );
                        self.status = self.update_check_status.clone();
                    }
                    UpdateCheckEvent::Found(set) => {
                        upsert_log(
                            &mut self.update_log,
                            set.beatmapset_id,
                            RepairLogStatus::InProgress,
                            format!(
                                "{} of {} diffs outdated",
                                set.outdated_count(),
                                set.total_checked()
                            ),
                        );
                        self.outdated_sets.push(set);
                    }
                    UpdateCheckEvent::Skipped {
                        beatmapset_id,
                        reason,
                    } => {
                        self.update_skipped += 1;
                        upsert_log(
                            &mut self.update_log,
                            beatmapset_id,
                            RepairLogStatus::Skipped,
                            reason,
                        );
                    }
                    UpdateCheckEvent::Unavailable {
                        beatmapset_id,
                        reason,
                    } => {
                        self.update_unavailable += 1;
                        if let Some(set_id) = beatmapset_id {
                            upsert_log(
                                &mut self.update_log,
                                set_id,
                                RepairLogStatus::Failed,
                                reason,
                            );
                        }
                    }
                    UpdateCheckEvent::Finished => {
                        self.is_checking_updates = false;
                        self.update_check_status = format!(
                            "Update check finished: {} outdated out of {} checked ({} unavailable, {} skipped, {} uncheckable)",
                            self.outdated_sets.len(),
                            self.update_check_total,
                            self.update_unavailable,
                            self.update_skipped,
                            self.update_check_uncheckable
                        );
                        self.status = self.update_check_status.clone();
                        keep_rx = false;
                    }
                }
            }

            if keep_rx {
                if disconnected {
                    self.is_checking_updates = false;
                    self.status = "Update check worker disconnected".to_owned();
                } else {
                    self.update_check_rx = Some(rx);
                }
            }
        }

        if let Some(rx) = self.update_rx.take() {
            let mut keep_rx = true;
            let mut disconnected = false;
            loop {
                let event = match rx.try_recv() {
                    Ok(event) => event,
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                };
                match event {
                    UpdateEvent::Started { total } => {
                        self.is_updating = true;
                        self.update_total = total;
                        self.update_done = 0;
                        self.update_successes = 0;
                        self.update_failures = 0;
                        self.update_touched_folders.clear();
                        self.update_progress = format!(
                            "Preparing to update {}",
                            plural(total, "beatmapset", "beatmapsets")
                        );
                        self.status = self.update_progress.clone();
                    }
                    UpdateEvent::Opening {
                        beatmapset_id,
                        index,
                        total,
                    } => {
                        upsert_log(
                            &mut self.update_log,
                            beatmapset_id,
                            RepairLogStatus::InProgress,
                            format!("Downloading {index}/{total}"),
                        );
                        self.update_progress =
                            format!("Downloading update {index}/{total}: set {beatmapset_id}");
                        self.status = self.update_progress.clone();
                    }
                    UpdateEvent::Updated {
                        beatmapset_id,
                        folders,
                        written,
                        removed,
                        download_source,
                        checksum_note,
                    } => {
                        self.update_done += 1;
                        self.update_successes += 1;
                        self.update_touched_folders.extend(folders.clone());
                        // A checksum note means the set installed cleanly but
                        // some difficulties still disagree with osu!web — the
                        // user should re-check later rather than see a hard
                        // failure for content the server itself served.
                        let mut message = format!(
                            "Updated via {download_source}: {} written, {} removed upstream",
                            plural(written, "file", "files"),
                            removed
                        );
                        if let Some(note) = &checksum_note {
                            message.push_str(&format!(" — {note}"));
                        }
                        upsert_log(
                            &mut self.update_log,
                            beatmapset_id,
                            RepairLogStatus::Success,
                            message,
                        );
                        self.update_progress = format!("Updated set {beatmapset_id}");
                        self.status = self.update_progress.clone();
                    }
                    UpdateEvent::Failed {
                        beatmapset_id,
                        message,
                    } => {
                        self.update_done += 1;
                        self.update_failures += 1;
                        upsert_log(
                            &mut self.update_log,
                            beatmapset_id,
                            RepairLogStatus::Failed,
                            message.clone(),
                        );
                        self.update_progress =
                            format!("Update failed for set {beatmapset_id}: {message}");
                        self.status = self.update_progress.clone();
                    }
                    UpdateEvent::Finished => {
                        self.is_updating = false;
                        self.outdated_sets.retain(|set| {
                            !self.update_log.iter().any(|entry| {
                                entry.beatmapset_id == set.beatmapset_id
                                    && matches!(entry.status, RepairLogStatus::Success)
                            })
                        });
                        self.status = format!(
                            "Update finished: {} succeeded, {} failed out of {}",
                            self.update_successes, self.update_failures, self.update_total
                        );
                        let touched = std::mem::take(&mut self.update_touched_folders);
                        if !touched.is_empty() {
                            self.prune_scan_folders(&touched);
                            self.status.push_str("; rescanning updated files");
                        }
                        keep_rx = false;
                        if !touched.is_empty() && !self.is_scanning {
                            self.start_scan();
                        }
                    }
                }
            }

            if keep_rx {
                if disconnected {
                    self.is_updating = false;
                    self.status = "Update worker disconnected".to_owned();
                } else {
                    self.update_rx = Some(rx);
                }
            }
        }
    }

    fn poll_background_load(&mut self, ctx: &egui::Context) {
        // Drain every completed decode (current preview plus prefetches) so a
        // burst of finished workers cannot stall behind a single try_recv.
        loop {
            match self.background_load_rx.try_recv() {
                Ok((path, result)) => {
                    self.background_in_flight.remove(&path);
                    match result {
                        Ok(image) => {
                            let texture = ctx.load_texture(
                                format!("map-background:{}", path.display()),
                                image,
                                egui::TextureOptions::LINEAR,
                            );
                            self.cache_background(path.clone(), texture.clone());
                            if self.background_preview_path.as_deref() == Some(&path) {
                                self.background_preview = Some(texture);
                                self.background_preview_error = None;
                            }
                        }
                        Err(_) => {
                            if self.background_preview_path.as_deref() == Some(&path) {
                                // Kept short on purpose: the underlying cause
                                // (unreadable/corrupt image) is not actionable
                                // in-app, and the long chain-typed error with
                                // the full path overflows the preview box.
                                self.background_preview_error =
                                    Some("Could not load background".to_owned());
                            }
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }
    }

    fn start_scan(&mut self) {
        if self.is_scanning {
            self.status = "A scan is already running".to_owned();
            return;
        }

        let songs_dir = expand_prefilled_path(&self.songs_dir);
        // The Songs folder is the single source of truth: when it points at a
        // different install, reload the per-library state (as picking a new
        // osu! root used to do).
        self.sync_library_state();
        let root = self.osu_root();
        let osu_root = (!root.trim().is_empty()).then(|| expand_prefilled_path(&root));
        let cached_scan = self.scan.clone();
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let mut skip_issue_kinds = BTreeSet::new();
        if self.skip_parse_timeouts {
            skip_issue_kinds.insert("parse_timeout".to_owned());
        }
        if self.skip_parse_errors {
            skip_issue_kinds.insert("parse_error".to_owned());
        }
        self.scanned_maps = 0;
        self.scan_total = 0;
        self.matched_maps = 0;
        self.status = scan_progress_status(self.scanned_maps, self.scan_total, self.matched_maps);
        std::thread::spawn(move || {
            local::scan_songs_dir_streaming(
                songs_dir,
                osu_root,
                cached_scan,
                worker_cancel,
                skip_issue_kinds,
                tx,
            );
        });
        self.scan_cancel = Some(cancel);
        self.scan_rx = Some(rx);
        self.background_cache.clear();
        self.background_cache_order.clear();
        self.background_in_flight.clear();
    }

    fn stop_scan(&mut self) {
        if let Some(cancel) = &self.scan_cancel {
            cancel.store(true, Ordering::Relaxed);
            // Leave `is_scanning` set until the worker answers with `Stopped`:
            // clearing it here would let a second scan start while the first
            // worker is still alive. Repeated clicks are idempotent.
            self.status = format!(
                "Scan stop requested: {}/{} maps read, {} match current filters",
                self.scanned_maps, self.scan_total, self.matched_maps
            );
        }
    }

    /// Records a collection action's outcome so it can be shown as an inline
    /// card on the Collections page (the bottom status bar alone was too easy
    /// to miss for the app's most important feedback).
    fn set_collection_notice(&mut self, ok: bool, message: String) {
        self.status = message.clone();
        self.collection_notice = Some(CollectionNotice { ok, message });
    }

    /// Renames a stored collection in place, keeping its maps. Distinct from
    /// "Save collection", which always writes the *current selection*.
    fn rename_collection(&mut self, old: &str, new: &str) {
        let new = new.trim();
        if new.is_empty() || new == old {
            return;
        }
        let Some(path) = self.collection_db_path() else {
            self.set_collection_notice(false, "Set your Songs folder first".to_owned());
            return;
        };
        let mut db = match collection::load_collection_db(&path) {
            Ok(db) => db,
            Err(err) => {
                self.set_collection_notice(
                    false,
                    format!("Rename failed: could not read collection.db ({err:#})"),
                );
                return;
            }
        };
        if db
            .collections
            .iter()
            .any(|collection| collection.name == new)
        {
            self.set_collection_notice(
                false,
                format!("A collection named \"{new}\" already exists"),
            );
            return;
        }
        let Some(entry) = db
            .collections
            .iter_mut()
            .find(|collection| collection.name == old)
        else {
            self.set_collection_notice(false, format!("Collection not found: {old}"));
            return;
        };
        entry.name = new.to_owned();
        match collection::write_db(&path, &db) {
            Ok(()) => {
                self.set_collection_notice(true, format!("Renamed \"{old}\" to \"{new}\""));
                // Auto-add settings are keyed by collection name — move them
                // along with the rename.
                if let Some(config) = self.auto_collections.collections.remove(old) {
                    self.auto_collections
                        .collections
                        .insert(new.to_owned(), config);
                    self.save_auto_collections();
                }
                self.load_collections();
                self.selected_collection_index = self
                    .collections
                    .iter()
                    .position(|collection| collection.name == new);
                self.collection_name = new.to_owned();
            }
            Err(err) => self.set_collection_notice(false, format!("Rename failed: {err:#}")),
        }
    }

    fn create_collection(&mut self, name: &str) {
        let Some(path) = self.collection_db_path() else {
            self.set_collection_notice(false, "Set your Songs folder first".to_owned());
            return;
        };
        match collection::create_collection(&path, name) {
            Ok(()) => {
                self.set_collection_notice(true, format!("Created empty collection \"{name}\""));
                self.load_collections();
                self.selected_collection_index = self
                    .collections
                    .iter()
                    .position(|collection| collection.name == name);
            }
            Err(err) => {
                self.set_collection_notice(false, format!("Create collection failed: {err:#}"))
            }
        }
    }

    fn add_selected_to_collection(&mut self, name: &str) {
        if self.selected_maps.is_empty() && self.selected_md5s.is_empty() {
            self.set_collection_notice(
                false,
                "Select at least one map before adding to a collection".to_owned(),
            );
            return;
        }

        let Some(path) = self.collection_db_path() else {
            self.set_collection_notice(false, "Set your Songs folder first".to_owned());
            return;
        };
        let mut hashes = self
            .selected_maps
            .iter()
            .map(|map| map.md5.clone())
            .collect::<Vec<_>>();
        for hash in &self.collection_missing_hashes {
            if self.selected_md5s.contains(hash) && !hashes.iter().any(|h| h == hash) {
                hashes.push(hash.clone());
            }
        }

        match collection::add_to_collection(&path, name, &hashes) {
            Ok(()) => {
                self.set_collection_notice(
                    true,
                    format!(
                        "Added {} {} to \"{name}\"",
                        hashes.len(),
                        if hashes.len() == 1 { "map" } else { "maps" }
                    ),
                );
                self.load_collections();
            }
            Err(err) => {
                self.set_collection_notice(false, format!("Add to collection failed: {err:#}"))
            }
        }
    }

    /// Appends the scan's newly discovered maps to every auto-add collection
    /// whose filters they match, then reloads the collection list. Runs on
    /// the UI thread right after a scan finishes — the same place the manual
    /// collection flows write `collection.db`.
    fn auto_add_new_maps(&mut self, new_maps: &[LocalBeatmap]) {
        let Some(path) = self.collection_db_path() else {
            return;
        };
        let enabled = self
            .auto_collections
            .collections
            .iter()
            .filter(|(_, config)| config.enabled)
            .map(|(name, config)| (name.clone(), config.clone()))
            .collect::<Vec<_>>();
        let mut summaries = Vec::new();
        for (name, config) in &enabled {
            let hashes = new_maps
                .iter()
                .filter(|map| config.filters.matches_local(map))
                .map(|map| map.md5.clone())
                .collect::<Vec<_>>();
            if hashes.is_empty() {
                continue;
            }
            match collection::add_to_collection(&path, name, &hashes) {
                Ok(()) => summaries.push(format!("\"{name}\": {}", hashes.len())),
                Err(err) => self.set_collection_notice(
                    false,
                    format!("Auto-add to \"{name}\" failed: {err:#}"),
                ),
            }
        }
        if summaries.is_empty() {
            return;
        }
        self.load_collections();
        self.set_collection_notice(
            true,
            format!(
                "Auto-added new maps matching filters — {}",
                summaries.join(", ")
            ),
        );
    }

    /// Persists the auto-add sidecar. Failures surface as a collection
    /// notice; the collections page is the only editor, so that is where
    /// they are visible.
    fn save_auto_collections(&mut self) {
        let path = auto_collections_path(&self.osu_root());
        if let Err(err) = self.auto_collections.save(&path) {
            self.set_collection_notice(false, format!("Saving auto-add settings failed: {err:#}"));
        }
    }

    /// Manual backfill for an auto-add collection: adds every library map
    /// matching its filters right now. `add_to_collection` dedups, so maps
    /// already in the collection are untouched.
    fn add_all_matching_to_collection(&mut self, name: &str) {
        let Some(config) = self.auto_collections.collections.get(name).cloned() else {
            return;
        };
        let Some(path) = self.collection_db_path() else {
            self.set_collection_notice(false, "Set your Songs folder first".to_owned());
            return;
        };
        let Some(scan) = &self.scan else {
            self.set_collection_notice(false, "Scan your library first".to_owned());
            return;
        };
        let hashes = scan
            .maps
            .iter()
            .filter(|map| !map.md5.is_empty())
            .filter(|map| config.filters.matches_local(map))
            .map(|map| map.md5.clone())
            .collect::<Vec<_>>();
        if hashes.is_empty() {
            self.set_collection_notice(
                false,
                format!("No library maps match \"{name}\"'s filters"),
            );
            return;
        }
        match collection::add_to_collection(&path, name, &hashes) {
            Ok(()) => {
                self.set_collection_notice(
                    true,
                    format!(
                        "Added {} matching {} to \"{name}\"",
                        hashes.len(),
                        if hashes.len() == 1 { "map" } else { "maps" }
                    ),
                );
                self.load_collections();
            }
            Err(err) => {
                self.set_collection_notice(false, format!("Add to collection failed: {err:#}"))
            }
        }
    }

    /// "N of M library maps match" for the auto-add editor, cached by
    /// serialized filters + scan generation so dragging a slider doesn't
    /// re-evaluate the whole library every frame.
    fn cached_auto_match_count(&mut self) -> (usize, usize) {
        let Some(picked) = self
            .selected_collection_index
            .and_then(|index| self.collections.get(index))
            .map(|collection| collection.name.clone())
        else {
            return (0, 0);
        };
        let Some(config) = self.auto_collections.collections.get(&picked) else {
            return (0, 0);
        };
        let key = (
            serde_json::to_string(&config.filters).unwrap_or_default(),
            self.scan_generation,
        );
        if self.auto_match_cache_key == key
            && let Some(count) = self.auto_match_cache_count
        {
            return (count, self.scan.as_ref().map_or(0, |scan| scan.maps.len()));
        }
        let Some(scan) = &self.scan else {
            return (0, 0);
        };
        let total = scan.maps.len();
        let matching = scan
            .maps
            .iter()
            .filter(|map| config.filters.matches_local(map))
            .count();
        self.auto_match_cache_key = key;
        self.auto_match_cache_count = Some(matching);
        (matching, total)
    }

    fn export_manifest(&mut self) {
        // Next to the scan cache and other app state — never the process
        // working directory, which may be read-only or surprising.
        let path = app_data_path(&self.osu_root()).join("selected_maps.tsv");
        if let Some(parent) = path.parent()
            && fs::create_dir_all(parent).is_err()
        {
            self.set_collection_notice(
                false,
                format!("Export failed: cannot create {}", parent.display()),
            );
            return;
        }
        match collection::write_manifest(&path, &self.selected_maps) {
            Ok(()) => self.set_collection_notice(
                true,
                format!("Exported the selected maps to {}", path.display()),
            ),
            Err(err) => self.set_collection_notice(false, format!("Export failed: {err:#}")),
        }
    }

    fn restore_collection_backup(&mut self) {
        let Some(path) = self.collection_db_path() else {
            self.set_collection_notice(false, "Set your Songs folder first".to_owned());
            return;
        };

        match collection::restore_collection_backup(&path) {
            Ok(()) => {
                self.set_collection_notice(
                    true,
                    format!("Restored the backup into {}", path.display()),
                );
                self.load_collections();
            }
            Err(err) => self.set_collection_notice(false, format!("Restore failed: {err:#}")),
        }
    }

    /// Path to osu!'s `collection.db`, or `None` when no Songs folder is set.
    /// There is intentionally no working-directory fallback: writing a
    /// `collection.db` next to whatever the process CWD happens to be would
    /// silently target the wrong library.
    fn collection_db_path(&self) -> Option<PathBuf> {
        let root = self.osu_root();
        if root.trim().is_empty() {
            None
        } else {
            Some(expand_prefilled_path(&root).join("collection.db"))
        }
    }

    /// osu! install root derived from the Songs folder (its parent when the
    /// folder itself is named `Songs`, otherwise the folder itself).
    fn osu_root(&self) -> String {
        derive_osu_root(&self.songs_dir)
    }

    /// Reloads per-library state when the Songs folder now points at a
    /// different install (sign-in session, repair ignores, cached scan).
    fn sync_library_state(&mut self) {
        let root = self.osu_root();
        if root == self.loaded_root {
            return;
        }
        self.loaded_root = root.clone();
        // Shrink reports and probe entries are folder-keyed: a different
        // library would render (and convert) stale paths.
        self.shrink_reports.clear();
        self.shrink_probe_cache.clear();
        self.shrink_backups.clear();
        self.shrink_log.clear();
        self.repair_ignores = load_repair_ignores(&root).unwrap_or_default();
        self.oauth_session = osu_oauth::load_oauth_session(&root);
        self.oauth_status = if self.oauth_session.is_some() {
            "Signed in with osu! (token loaded from disk)".to_owned()
        } else {
            "Not signed in with osu!".to_owned()
        };
        self.collections.clear();
        self.selected_collection_index = None;
        self.collection_missing_hashes.clear();
        self.scan = load_scan_cache(&root).ok().flatten();
        self.auto_collections =
            collection::AutoCollectionStore::load(&auto_collections_path(&root));
        self.last_scan_md5s = scan_md5_baseline(self.scan.as_ref());
        self.auto_match_cache_key = (String::new(), 0);
        self.auto_match_cache_count = None;
        self.expanded_map_md5 = None;
        self.clear_background_preview();
        self.invalidate_scan_caches();
    }

    fn load_collections(&mut self) {
        let Some(path) = self.collection_db_path() else {
            self.collections.clear();
            self.selected_collection_index = None;
            self.collection_missing_hashes.clear();
            self.status = "Set your Songs folder first".to_owned();
            return;
        };
        match collection::load_collection_db(&path) {
            Ok(db) => {
                let count = db.collections.len();
                self.collections = db.collections;
                self.selected_collection_index = self
                    .selected_collection_index
                    .filter(|&index| index < self.collections.len());
                self.collection_missing_hashes.clear();
                self.status = format!(
                    "Loaded {} from {}",
                    plural(count, "collection", "collections"),
                    path.display()
                );
            }
            Err(err) => {
                self.collections.clear();
                self.selected_collection_index = None;
                self.collection_missing_hashes.clear();
                self.status = format!("Collection load failed: {err:#}");
            }
        }
    }

    fn load_selected_collection_into_selection(&mut self) {
        let Some(collection) = self
            .selected_collection_index
            .and_then(|index| self.collections.get(index))
            .cloned()
        else {
            self.status = "Select a collection to load".to_owned();
            return;
        };

        self.collection_name = collection.name.clone();
        self.selected_maps.clear();
        self.selected_md5s = collection.hashes.iter().cloned().collect();
        self.collection_missing_hashes.clear();

        if let Some(scan) = &self.scan {
            let by_hash = scan
                .maps
                .iter()
                .map(|map| (map.md5.as_str(), map))
                .collect::<BTreeMap<_, _>>();
            for hash in &collection.hashes {
                if let Some(map) = by_hash.get(hash.as_str()) {
                    self.selected_maps.push((*map).clone());
                } else {
                    self.collection_missing_hashes.push(hash.clone());
                }
            }
            let missing = self.collection_missing_hashes.len();
            self.set_collection_notice(
                true,
                if missing > 0 {
                    format!(
                        "Opened \"{picked}\" — {} maps. {missing} in the collection {} not installed locally and {} kept.",
                        self.selected_maps.len(),
                        if missing == 1 { "is" } else { "are" },
                        if missing == 1 { "is" } else { "are" },
                        picked = collection.name,
                    )
                } else {
                    format!(
                        "Opened \"{picked}\" — {} maps.",
                        self.selected_maps.len(),
                        picked = collection.name
                    )
                },
            );
        } else {
            self.collection_missing_hashes = collection.hashes.clone();
            self.set_collection_notice(
                true,
                format!(
                    "Opened \"{picked}\" — load your maps in the Library tab to match its {} entries.",
                    collection.hashes.len(),
                    picked = collection.name
                ),
            );
        }
    }

    fn save_selection_to_collection(&mut self) {
        let Some(path) = self.collection_db_path() else {
            self.status = "Set your Songs folder first".to_owned();
            return;
        };
        let original_name = self
            .selected_collection_index
            .and_then(|index| self.collections.get(index))
            .map(|collection| collection.name.clone());
        let mut db = match collection::load_collection_db(&path) {
            Ok(db) => db,
            Err(err) if !path.exists() => {
                let _ = err;
                collection::CollectionDb::empty()
            }
            Err(err) => {
                self.status = format!("Collection save failed: {err:#}");
                return;
            }
        };

        if let Some(original_name) = original_name.as_deref()
            && original_name != self.collection_name
        {
            collection::delete_collection(&mut db, original_name);
        }

        let mut hashes = self
            .selected_maps
            .iter()
            .map(|map| map.md5.clone())
            .collect::<Vec<_>>();
        for hash in &self.collection_missing_hashes {
            if !self.selected_md5s.contains(hash) {
                continue;
            }
            if !hashes.iter().any(|existing| existing == hash) {
                hashes.push(hash.clone());
            }
        }

        let saved_count = hashes.len();
        collection::upsert_collection_hashes(&mut db, &self.collection_name, hashes);
        match collection::write_db(&path, &db) {
            Ok(()) => {
                self.set_collection_notice(
                    true,
                    format!(
                        "Saved collection \"{}\" — {}",
                        self.collection_name,
                        plural(saved_count, "map", "maps")
                    ),
                );
                self.load_collections();
                self.selected_collection_index = self
                    .collections
                    .iter()
                    .position(|collection| collection.name == self.collection_name);
                // This save renamed the collection (delete-and-recreate):
                // move any auto-add settings to the new name.
                if let Some(original_name) = original_name.as_deref()
                    && original_name != self.collection_name
                    && let Some(config) = self.auto_collections.collections.remove(original_name)
                {
                    self.auto_collections
                        .collections
                        .insert(self.collection_name.clone(), config);
                    self.save_auto_collections();
                }
            }
            Err(err) => {
                self.set_collection_notice(false, format!("Collection save failed: {err:#}"))
            }
        }
    }

    fn delete_selected_collection(&mut self) {
        let Some(collection_name) = self
            .selected_collection_index
            .and_then(|index| self.collections.get(index))
            .map(|collection| collection.name.clone())
        else {
            self.status = "Select a collection to delete".to_owned();
            return;
        };

        let Some(path) = self.collection_db_path() else {
            self.status = "Set your Songs folder first".to_owned();
            return;
        };
        let mut db = match collection::load_collection_db(&path) {
            Ok(db) => db,
            Err(err) => {
                self.status = format!("Collection delete failed: {err:#}");
                return;
            }
        };

        if !collection::delete_collection(&mut db, &collection_name) {
            self.status = format!("Collection not found: {collection_name}");
            return;
        }

        match collection::write_db(&path, &db) {
            Ok(()) => {
                self.set_collection_notice(
                    true,
                    format!("Deleted collection \"{collection_name}\""),
                );
                self.selected_collection_index = None;
                self.collection_missing_hashes.clear();
                if self
                    .auto_collections
                    .collections
                    .remove(&collection_name)
                    .is_some()
                {
                    self.save_auto_collections();
                }
                self.load_collections();
            }
            Err(err) => {
                self.set_collection_notice(false, format!("Collection delete failed: {err:#}"))
            }
        }
    }

    fn delete_selected_non_std_modes(&mut self) {
        let selection = DeleteModeSelection {
            taiko: self.delete_taiko,
            catch: self.delete_catch,
            mania: self.delete_mania,
        };
        if !selection.any() {
            self.status = "Select at least one non-std mode to delete".to_owned();
            return;
        }

        let Some(scan) = &mut self.scan else {
            self.status = "Scan your Songs directory before deleting non-std maps".to_owned();
            return;
        };

        let targets = scan
            .maps
            .iter()
            .filter(|map| selection.matches(map.mode))
            .map(|map| map.path.clone())
            .collect::<BTreeSet<_>>();
        if targets.is_empty() {
            self.status = "No scanned maps match the selected non-std modes".to_owned();
            return;
        }

        let mut deleted = BTreeSet::new();
        let mut failures = Vec::new();
        for path in &targets {
            match fs::remove_file(path) {
                Ok(()) => {
                    deleted.insert(path.clone());
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    deleted.insert(path.clone());
                }
                Err(err) => failures.push(format!("{}: {err}", path.display())),
            }
        }

        let mut removed_folders = 0_usize;
        if !deleted.is_empty() {
            scan.maps.retain(|map| !deleted.contains(&map.path));
            scan.file_meta.retain(|path, _| !deleted.contains(path));
            scan.problems
                .retain(|issue| !deleted.contains(&issue.beatmap));
            scan.sets = build_sets_for_scan(&scan.maps);
            let mode = self.filters.mode;
            self.retain_selected_maps(|map| !deleted.contains(&map.path) && mode.matches(map.mode));
            self.invalidate_scan_caches();
            removed_folders = remove_empty_folders(
                deleted
                    .iter()
                    .filter_map(|path| path.parent().map(Path::to_path_buf)),
            );
        }

        self.status = if failures.is_empty() {
            format!(
                "Deleted {} non-std {}{}",
                deleted.len(),
                if deleted.len() == 1 {
                    "difficulty file"
                } else {
                    "difficulty files"
                },
                if removed_folders > 0 {
                    format!(
                        ", removed {removed_folders} empty {}",
                        if removed_folders == 1 {
                            "folder"
                        } else {
                            "folders"
                        }
                    )
                } else {
                    String::new()
                }
            )
        } else {
            format!(
                "Deleted {} non-std difficulty files; {} failed: {}",
                deleted.len(),
                failures.len(),
                failures.join("; ")
            )
        };
    }

    fn start_repair_all(&mut self) {
        if self.is_repairing {
            self.status = "A repair job is already running".to_owned();
            return;
        }

        if self.scan.is_none() {
            self.status = "Scan your Songs directory before repairing maps".to_owned();
            return;
        }

        self.refresh_repair_jobs_now();
        let jobs = self.repair_jobs_cache.clone();
        if jobs.is_empty() {
            self.status = "No repairable corrupted beatmapsets found".to_owned();
            return;
        }

        self.spawn_repair_jobs(jobs);
    }

    fn start_repair_single(&mut self, beatmapset_id: i64) {
        if self.is_repairing {
            self.status = "A repair job is already running".to_owned();
            return;
        }

        self.refresh_repair_jobs_now();
        let Some(job) = self
            .repair_jobs_cache
            .iter()
            .find(|job| job.beatmapset_id == beatmapset_id)
            .cloned()
        else {
            self.status = format!("No repair job found for set {beatmapset_id}");
            return;
        };

        self.spawn_repair_jobs(vec![job]);
    }

    fn spawn_repair_jobs(&mut self, jobs: Vec<RepairJob>) {
        let (tx, rx) = mpsc::channel();
        self.status = format!(
            "Starting repair for {}",
            plural(jobs.len(), "beatmapset", "beatmapsets")
        );
        let backend_url = osu_oauth::backend_url();
        let osu_root = self.osu_root();
        let oauth_session = self.oauth_session.clone();
        std::thread::spawn(move || {
            run_repair_jobs(jobs, backend_url, osu_root, oauth_session, tx);
        });
        self.repair_rx = Some(rx);
    }

    fn shrink_backup_dir(&self) -> PathBuf {
        app_data_path(&self.osu_root()).join("shrink-backups")
    }

    /// Analyze every scanned set folder for shrinkable assets. Results
    /// stream in per set; sets with work are selected by default.
    fn start_shrink_analysis(&mut self) {
        if self.is_analyzing || self.is_shrinking {
            self.status = "Shrink analysis or run already in progress".to_owned();
            return;
        }
        let Some(scan) = &self.scan else {
            self.status = "Scan your Songs directory before shrinking".to_owned();
            return;
        };
        if scan.sets.is_empty() {
            self.status = "No beatmapsets scanned yet".to_owned();
            return;
        }
        let bins = shrink::resolve_bins(ffmpeg_executable());
        if bins.ffprobe.is_none() {
            self.status =
                "ffprobe not found next to ffmpeg — audio/video plans will skip; images still work"
                    .to_owned();
        }
        let targets: Vec<(PathBuf, String, Vec<LocalBeatmap>)> = scan
            .sets
            .iter()
            .map(|set| {
                let label = set
                    .folder
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("set")
                    .to_owned();
                (set.folder.clone(), label, set.maps.clone())
            })
            .collect();
        let options = self.shrink_options.clone();
        let probe_cache = std::mem::take(&mut self.shrink_probe_cache);
        let (tx, rx) = mpsc::channel();
        self.analysis_rx = Some(rx);
        self.is_analyzing = true;
        self.analysis_done = 0;
        self.analysis_total = targets.len();
        // Library analysis replaces set reports but keeps appended skin
        // reports (different root, different lifecycle).
        if let Some(skins_dir) = skins_dir_for(&self.osu_root()) {
            self.shrink_reports
                .retain(|r| r.folder.starts_with(&skins_dir));
        } else {
            self.shrink_reports.clear();
        }
        self.status = format!(
            "Checking {}…",
            plural(targets.len(), "set folder", "set folders")
        );
        // Already-shrunk files are skipped via the on-disk shrink cache,
        // loaded once here and shared read-only by every worker.
        let shrink_cache = shrink::ShrinkCache::load(&shrink_cache_path(&self.osu_root()));
        if shrink_cache.len() > 0 {
            self.status = format!(
                "Checking set folders ({} remembered as already shrunk)…",
                shrink_cache.len()
            );
        }
        let shrink_cache = Arc::new(shrink_cache);
        let analysis_cancel = Arc::new(AtomicBool::new(false));
        self.analysis_cancel = Some(analysis_cancel.clone());
        let analysis_pause = Arc::new(AtomicBool::new(false));
        self.analysis_pause = Some(analysis_pause.clone());
        std::thread::spawn(move || {
            let _ = tx.send(ShrinkAnalysisEvent::Started {
                sets: targets.len(),
            });
            // Worker pool over set folders: ffprobe is process-spawn
            // bound, so several in flight hide the latency almost
            // linearly. Each worker owns a private cache shard; they
            // merge at the end (no lock contention on the hot path).
            let workers = std::thread::available_parallelism()
                .map(|n| n.get().min(8))
                .unwrap_or(4)
                .min(targets.len().max(1));
            let next = Arc::new(AtomicUsize::new(0));
            let targets = Arc::new(targets);
            std::thread::scope(|scope| {
                let mut shards = Vec::with_capacity(workers);
                for _ in 0..workers {
                    let tx = tx.clone();
                    let targets = targets.clone();
                    let next = next.clone();
                    let bins = bins.clone();
                    let options = options.clone();
                    let shrink_cache = shrink_cache.clone();
                    let cancel = analysis_cancel.clone();
                    let pause = analysis_pause.clone();
                    shards.push(scope.spawn(move || {
                        let mut local = shrink::ProbeCache::new();
                        loop {
                            if cancel.load(Ordering::Relaxed) {
                                break;
                            }
                            if !shrink::wait_while_paused(&pause, &cancel) {
                                break;
                            }
                            let index = next.fetch_add(1, Ordering::Relaxed);
                            let Some((folder, label, maps)) = targets.get(index) else {
                                break;
                            };
                            let report = shrink::analyze_set(
                                folder,
                                label.clone(),
                                maps,
                                &bins,
                                &options,
                                &mut local,
                                &shrink_cache,
                            );
                            if tx.send(ShrinkAnalysisEvent::Report { report }).is_err() {
                                break;
                            }
                        }
                        local
                    }));
                }
                let mut merged = probe_cache;
                for shard in shards {
                    if let Ok(local) = shard.join() {
                        merged.extend(local);
                    }
                }
                let _ = tx.send(ShrinkAnalysisEvent::Finished { cache: merged });
            });
        });
    }

    /// Append skin reports without touching set reports. Skins live
    /// under `<osu root>/Skins`; each subfolder is one skin.
    fn start_skin_analysis(&mut self) {
        if self.is_analyzing || self.is_shrinking {
            self.status = "Shrink analysis or run already in progress".to_owned();
            return;
        }
        let Some(skins_dir) = skins_dir_for(&self.osu_root()) else {
            self.status = "Set your Songs folder first so the Skins folder can be found".to_owned();
            return;
        };
        if !skins_dir.is_dir() {
            self.status = format!("No Skins folder next to Songs ({})", skins_dir.display());
            return;
        }
        let mut targets: Vec<(PathBuf, String)> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&skins_dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let label = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("skin")
                    .to_owned();
                targets.push((path, format!("🎨 {label}")));
            }
        }
        targets.sort();
        if targets.is_empty() {
            self.status = "No skins found".to_owned();
            return;
        }
        // Skip skins already analyzed this session.
        targets.retain(|(folder, _)| !self.shrink_reports.iter().any(|r| &r.folder == folder));
        if targets.is_empty() {
            self.status = "All skins already analyzed".to_owned();
            return;
        }
        let probe_cache = std::mem::take(&mut self.shrink_probe_cache);
        let options = self.shrink_options.clone();
        let shrink_cache = Arc::new(shrink::ShrinkCache::load(&shrink_cache_path(
            &self.osu_root(),
        )));
        let analysis_cancel = Arc::new(AtomicBool::new(false));
        self.analysis_cancel = Some(analysis_cancel.clone());
        let analysis_pause = Arc::new(AtomicBool::new(false));
        self.analysis_pause = Some(analysis_pause.clone());
        let (tx, rx) = mpsc::channel();
        self.analysis_rx = Some(rx);
        self.is_analyzing = true;
        self.analysis_done = 0;
        self.analysis_total = targets.len();
        self.status = format!("Checking {}…", plural(targets.len(), "skin", "skins"));
        std::thread::spawn(move || {
            let _ = tx.send(ShrinkAnalysisEvent::Started {
                sets: targets.len(),
            });
            for (folder, label) in targets {
                if analysis_cancel.load(Ordering::Relaxed) {
                    break;
                }
                if !shrink::wait_while_paused(&analysis_pause, &analysis_cancel) {
                    break;
                }
                // Skins never probe (image headers only), so the probe
                // cache passes through untouched.
                let report = shrink::analyze_skin(&folder, label, &options, &shrink_cache);
                if tx.send(ShrinkAnalysisEvent::Report { report }).is_err() {
                    break;
                }
            }
            let _ = tx.send(ShrinkAnalysisEvent::Finished { cache: probe_cache });
        });
    }

    fn start_shrink_run(&mut self) {
        if self.is_shrinking || self.is_analyzing {
            self.status = "A shrink run or analysis is already in progress".to_owned();
            return;
        }
        let selected: Vec<SetShrinkReport> = self
            .shrink_reports
            .iter()
            .filter(|r| r.work_items() > 0)
            .cloned()
            .collect();
        if selected.is_empty() {
            self.status = "Nothing to shrink — analyze first".to_owned();
            return;
        }
        if self.scan.is_none() {
            self.status = "Scan your Songs directory before shrinking".to_owned();
            return;
        }
        let mut jobs = Vec::new();
        for report in selected {
            jobs.push(ShrinkJob { report });
        }
        let bins = shrink::resolve_bins(ffmpeg_executable());
        let backup_dir = self.shrink_backup_dir();
        let cache_file = shrink_cache_path(&self.osu_root());
        let options = self.shrink_options.clone();
        let delete_orphans = options.delete_orphans;
        let cancel = Arc::new(AtomicBool::new(false));
        self.shrink_cancel = Some(cancel.clone());
        let pause = Arc::new(AtomicBool::new(false));
        self.shrink_pause = Some(pause.clone());
        let (tx, rx) = mpsc::channel();
        self.shrink_rx = Some(rx);
        self.is_shrinking = true;
        self.status = format!("Starting shrink for {}", plural(jobs.len(), "set", "sets"));
        std::thread::spawn(move || {
            shrink::run_shrink_jobs(
                jobs,
                bins,
                options,
                backup_dir,
                delete_orphans,
                cancel,
                pause,
                tx,
                cache_file,
            );
        });
    }

    fn stop_shrink(&mut self) {
        if let Some(cancel) = self.shrink_cancel.as_ref() {
            cancel.store(true, Ordering::Relaxed);
            self.status = "Stopping shrink after the current file…".to_owned();
        }
    }

    fn stop_shrink_analysis(&mut self) {
        if let Some(cancel) = self.analysis_cancel.as_ref() {
            cancel.store(true, Ordering::Relaxed);
            self.status = "Stopping analysis after the current folder…".to_owned();
        }
    }

    fn toggle_analysis_pause(&mut self) {
        if let Some(pause) = self.analysis_pause.as_ref() {
            let paused = !pause.load(Ordering::Relaxed);
            pause.store(paused, Ordering::Relaxed);
            self.status = if paused {
                "Analysis paused — resume to continue".to_owned()
            } else {
                "Analysis resumed".to_owned()
            };
        }
    }

    fn toggle_shrink_pause(&mut self) {
        if let Some(pause) = self.shrink_pause.as_ref() {
            let paused = !pause.load(Ordering::Relaxed);
            pause.store(paused, Ordering::Relaxed);
            self.status = if paused {
                "Shrink paused after the current file — resume to continue".to_owned()
            } else {
                "Shrink resumed".to_owned()
            };
        }
    }

    fn toggle_update_pause(&mut self) {
        if let Some(pause) = self.update_pause.as_ref() {
            let paused = !pause.load(Ordering::Relaxed);
            pause.store(paused, Ordering::Relaxed);
            self.status = if paused {
                "Update paused after the current set — resume to continue".to_owned()
            } else {
                "Update resumed".to_owned()
            };
        }
    }

    fn update_paused(&self) -> bool {
        self.update_pause
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
    }

    fn stop_update(&mut self) {
        if let Some(cancel) = self.update_cancel.as_ref() {
            cancel.store(true, Ordering::Relaxed);
            self.status = "Stopping update after the current set…".to_owned();
        }
    }

    fn toggle_update_check_pause(&mut self) {
        if let Some(pause) = self.update_check_pause.as_ref() {
            let paused = !pause.load(Ordering::Relaxed);
            pause.store(paused, Ordering::Relaxed);
            self.status = if paused {
                "Update check paused — resume to continue".to_owned()
            } else {
                "Update check resumed".to_owned()
            };
        }
    }

    fn update_check_paused(&self) -> bool {
        self.update_check_pause
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
    }

    fn stop_update_check(&mut self) {
        if let Some(cancel) = self.update_check_cancel.as_ref() {
            cancel.store(true, Ordering::Relaxed);
            self.status = "Stopping update check after the current request…".to_owned();
        }
    }

    fn analysis_paused(&self) -> bool {
        self.analysis_pause
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
    }

    fn shrink_paused(&self) -> bool {
        self.shrink_pause
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
    }

    fn upsert_shrink_log(&mut self, label: String, status: RepairLogStatus, message: String) {
        if let Some(entry) = self.shrink_log.iter_mut().find(|e| e.label == label) {
            entry.status = status;
            entry.message = message;
        } else {
            self.shrink_log.push(JobLogEntry {
                label,
                status,
                message,
            });
        }
    }

    fn upsert_extras_log(&mut self, label: String, status: RepairLogStatus, message: String) {
        if let Some(entry) = self.extras_log.iter_mut().find(|e| e.label == label) {
            entry.status = status;
            entry.message = message;
        } else {
            self.extras_log.push(JobLogEntry {
                label,
                status,
                message,
            });
        }
    }

    // ── Extras: bulk background replacement ──

    fn pick_extras_image(&mut self) {
        if self.extras_running || self.extras_rolling_back || self.extras_image_rx.is_some() {
            return;
        }
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Images", extras::SUPPORTED_EXTS)
            .pick_file()
        else {
            return;
        };
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("image")
            .to_owned();
        self.extras_image_name = Some(name);
        self.extras_image_bytes = None;
        self.extras_image_ext = None;
        self.extras_image_texture = None;
        self.extras_image_size = None;
        let (tx, rx) = mpsc::channel();
        self.extras_image_rx = Some(rx);
        self.status = format!(
            "Importing {}…",
            self.extras_image_name.as_deref().unwrap_or("image")
        );
        std::thread::spawn(move || {
            let _ = tx.send(load_extras_image(&path));
        });
    }

    fn clear_extras_image(&mut self) {
        self.extras_image_bytes = None;
        self.extras_image_ext = None;
        self.extras_image_name = None;
        self.extras_image_texture = None;
        self.extras_image_size = None;
    }

    /// One folder per distinct beatmapset folder in the scan, sorted so the
    /// run order is stable and the progress bar moves predictably.
    fn extras_target_folders(&self) -> Vec<PathBuf> {
        self.scan
            .as_ref()
            .map(|scan| {
                scan.maps
                    .iter()
                    .map(|map| map.folder.clone())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect()
            })
            .unwrap_or_default()
    }

    fn start_extras_apply(&mut self) {
        if self.extras_running || self.extras_rolling_back {
            self.status = "A background job is already running".to_owned();
            return;
        }
        if self.is_scanning {
            self.status = "Wait for the scan to finish before applying".to_owned();
            return;
        }
        let Some(image) = self.extras_image_bytes.clone() else {
            self.status = "Import an image first".to_owned();
            return;
        };
        let Some(ext) = self.extras_image_ext.clone() else {
            self.status = "Import an image first".to_owned();
            return;
        };
        let folders = self.extras_target_folders();
        if folders.is_empty() {
            self.status = "No beatmapsets to update — scan your library first".to_owned();
            return;
        }
        // One rollback manifest per apply, named by timestamp so the newest
        // job is the lexicographic winner when looking for it later.
        let backups = extras_backup_dir(&self.osu_root());
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
        let mut job_id = stamp.clone();
        let mut manifest_path = backups.join(format!("{job_id}.json"));
        let mut suffix = 2;
        while manifest_path.exists() {
            job_id = format!("{stamp}-{suffix}");
            manifest_path = backups.join(format!("{job_id}.json"));
            suffix += 1;
        }
        let image_name = self.extras_image_name.clone().unwrap_or_default();
        let cache_file = extras_cache_path(&self.osu_root());
        let cancel = Arc::new(AtomicBool::new(false));
        self.extras_cancel = Some(cancel.clone());
        let (tx, rx) = mpsc::channel();
        self.extras_rx = Some(rx);
        self.extras_running = true;
        self.status = format!(
            "Starting background replacement for {}",
            plural(folders.len(), "set folder", "set folders")
        );
        std::thread::spawn(move || {
            extras::run_background_jobs(
                folders,
                image,
                ext,
                image_name,
                cancel,
                tx,
                backups.join(&job_id),
                manifest_path,
                cache_file,
            );
        });
    }

    fn stop_extras(&mut self) {
        if let Some(cancel) = self.extras_cancel.as_ref() {
            cancel.store(true, Ordering::Relaxed);
            self.status = "Stopping after the current folder…".to_owned();
        }
    }

    fn start_extras_rollback(&mut self) {
        if self.extras_running || self.extras_rolling_back {
            self.status = "A background job is already running".to_owned();
            return;
        }
        if self.is_scanning {
            self.status = "Wait for the scan to finish before rolling back".to_owned();
            return;
        }
        let Some(job) = self.extras_last_job.clone() else {
            self.status = "No background change to undo".to_owned();
            return;
        };
        let cancel = Arc::new(AtomicBool::new(false));
        self.rollback_cancel = Some(cancel.clone());
        let (tx, rx) = mpsc::channel();
        self.rollback_rx = Some(rx);
        self.extras_rolling_back = true;
        self.status = format!("Undoing the background change from {}…", job.when);
        std::thread::spawn(move || {
            extras::run_rollback_job(job.manifest_path, cancel, tx);
        });
    }

    fn stop_extras_rollback(&mut self) {
        if let Some(cancel) = self.rollback_cancel.as_ref() {
            cancel.store(true, Ordering::Relaxed);
            self.status = "Stopping rollback after the current folder…".to_owned();
        }
    }

    fn poll_extras(&mut self, ctx: &egui::Context) {
        // Imported-image decode result: exactly one message per pick.
        if let Some(rx) = self.extras_image_rx.take() {
            match rx.try_recv() {
                Ok(Ok(image)) => {
                    let texture = ctx.load_texture(
                        "extras-background",
                        image.preview.clone(),
                        egui::TextureOptions::LINEAR,
                    );
                    self.extras_image_texture = Some(texture);
                    self.extras_image_bytes = Some(image.bytes);
                    self.extras_image_ext = Some(image.ext);
                    self.extras_image_size = Some(image.size);
                    let (width, height) = image.size;
                    self.status = format!(
                        "Imported {} ({}×{}, {} bytes) — ready to apply",
                        self.extras_image_name.as_deref().unwrap_or("image"),
                        width,
                        height,
                        self.extras_image_bytes
                            .as_ref()
                            .map_or(0, |bytes| bytes.len())
                    );
                }
                Ok(Err(err)) => {
                    self.clear_extras_image();
                    self.status = format!("Image import failed: {err:#}");
                }
                Err(mpsc::TryRecvError::Empty) => self.extras_image_rx = Some(rx),
                Err(mpsc::TryRecvError::Disconnected) => {}
            }
        }

        // Background-replacement job events.
        if let Some(rx) = self.extras_rx.take() {
            let mut keep_rx = true;
            let mut disconnected = false;
            loop {
                let event = match rx.try_recv() {
                    Ok(event) => event,
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                };
                match event {
                    ExtrasEvent::Started { folders } => {
                        self.extras_folders_total = folders;
                        self.extras_folders_done = 0;
                        self.extras_files_replaced = 0;
                        self.extras_files_cached = 0;
                        self.extras_files_skipped = 0;
                        self.extras_successes = 0;
                        self.extras_failures = 0;
                        self.extras_log.clear();
                        self.extras_touched_folders.clear();
                        self.extras_progress = format!(
                            "Applying background to {}…",
                            plural(folders, "set folder", "set folders")
                        );
                        self.status = self.extras_progress.clone();
                    }
                    ExtrasEvent::FolderDone {
                        folder,
                        files,
                        cached,
                        skipped,
                    } => {
                        self.extras_folders_done += 1;
                        self.extras_successes += 1;
                        self.extras_files_replaced += files;
                        self.extras_files_cached += cached;
                        self.extras_files_skipped += skipped;
                        self.extras_touched_folders.insert(folder.clone());
                        let label = folder
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("set")
                            .to_owned();
                        let mut message = format!(
                            "{} replaced",
                            plural(files, "background file", "background files")
                        );
                        if cached > 0 {
                            message.push_str(&format!(" · {cached} already up to date"));
                        }
                        if skipped > 0 {
                            message.push_str(&format!(" · {skipped} skipped (format)"));
                        }
                        self.upsert_extras_log(label, RepairLogStatus::Success, message);
                        self.extras_progress = format!(
                            "Applying background… {}/{}",
                            self.extras_folders_done,
                            plural(self.extras_folders_total, "folder", "folders")
                        );
                    }
                    ExtrasEvent::FolderFailed { folder, message } => {
                        self.extras_folders_done += 1;
                        self.extras_failures += 1;
                        self.extras_touched_folders.insert(folder.clone());
                        let label = folder
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("set")
                            .to_owned();
                        self.upsert_extras_log(label, RepairLogStatus::Failed, message.clone());
                        self.extras_progress = format!(
                            "Applying background… {}/{} — last failed: {message}",
                            self.extras_folders_done,
                            plural(self.extras_folders_total, "folder", "folders")
                        );
                    }
                    ExtrasEvent::Failed { message } => {
                        self.extras_progress = format!("Failed: {message}");
                        self.status = self.extras_progress.clone();
                    }
                    ExtrasEvent::Finished { files, elapsed_s } => {
                        self.extras_running = false;
                        let cancelled = self
                            .extras_cancel
                            .as_ref()
                            .is_some_and(|flag| flag.load(Ordering::Relaxed));
                        let skipped_note = if self.extras_files_skipped > 0 {
                            format!(
                                ", {} skipped (unsupported format)",
                                plural(self.extras_files_skipped, "file", "files")
                            )
                        } else {
                            String::new()
                        };
                        let cached_note = if self.extras_files_cached > 0 {
                            format!(", {} already up to date (cached)", self.extras_files_cached)
                        } else {
                            String::new()
                        };
                        self.status = format!(
                            "Backgrounds {} in {:.0}s: {} ok, {} failed, {} replaced{}{} — rescanning",
                            if cancelled { "cancelled" } else { "replaced" },
                            elapsed_s,
                            plural(self.extras_successes, "folder", "folders"),
                            plural(self.extras_failures, "folder", "folders"),
                            plural(files, "image file", "image files"),
                            cached_note,
                            skipped_note,
                        );
                        let touched = std::mem::take(&mut self.extras_touched_folders);
                        if !touched.is_empty() {
                            self.prune_scan_folders(&touched);
                        }
                        keep_rx = false;
                        if !touched.is_empty() && !self.is_scanning {
                            self.start_scan();
                        }
                        self.refresh_last_extras_job();
                    }
                }
            }
            if keep_rx {
                if disconnected {
                    self.extras_running = false;
                    self.status = "Background replacement worker disconnected".to_owned();
                } else {
                    self.extras_rx = Some(rx);
                }
            }
        }
    }

    fn poll_extras_rollback(&mut self) {
        let Some(rx) = self.rollback_rx.take() else {
            return;
        };
        let mut keep_rx = true;
        let mut disconnected = false;
        loop {
            let event = match rx.try_recv() {
                Ok(event) => event,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            };
            match event {
                RollbackEvent::Started { folders } => {
                    self.rollback_folders_total = folders;
                    self.rollback_folders_done = 0;
                    self.rollback_restored = 0;
                    self.rollback_successes = 0;
                    self.rollback_failures = 0;
                    self.extras_log.clear();
                    self.rollback_touched_folders.clear();
                    self.rollback_progress = format!(
                        "Undoing backgrounds… {}",
                        plural(folders, "set folder", "set folders")
                    );
                    self.status = self.rollback_progress.clone();
                }
                RollbackEvent::FolderDone { folder, restored } => {
                    self.rollback_folders_done += 1;
                    self.rollback_successes += 1;
                    self.rollback_restored += restored;
                    self.rollback_touched_folders.insert(folder.clone());
                    let label = folder
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("set")
                        .to_owned();
                    self.upsert_extras_log(
                        label,
                        RepairLogStatus::Success,
                        format!(
                            "{} restored",
                            plural(restored, "background file", "background files")
                        ),
                    );
                    self.rollback_progress = format!(
                        "Undoing backgrounds… {}/{}",
                        self.rollback_folders_done,
                        plural(self.rollback_folders_total, "folder", "folders")
                    );
                }
                RollbackEvent::FolderFailed { folder, message } => {
                    self.rollback_folders_done += 1;
                    self.rollback_failures += 1;
                    self.rollback_touched_folders.insert(folder.clone());
                    let label = folder
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("set")
                        .to_owned();
                    self.upsert_extras_log(label, RepairLogStatus::Failed, message.clone());
                    self.rollback_progress = format!(
                        "Undoing backgrounds… {}/{} — last failed: {message}",
                        self.rollback_folders_done,
                        plural(self.rollback_folders_total, "folder", "folders")
                    );
                }
                RollbackEvent::Failed { message } => {
                    self.rollback_progress = format!("Failed: {message}");
                    self.status = self.rollback_progress.clone();
                }
                RollbackEvent::Finished {
                    restored,
                    cleaned,
                    elapsed_s,
                } => {
                    self.extras_rolling_back = false;
                    let cancelled = self
                        .rollback_cancel
                        .as_ref()
                        .is_some_and(|flag| flag.load(Ordering::Relaxed));
                    let kept_note = if cleaned {
                        String::new()
                    } else {
                        " — undo data kept".to_owned()
                    };
                    self.status = format!(
                        "Undo {} in {:.0}s: {} ok, {} failed, {} restored{} — rescanning",
                        if cancelled { "cancelled" } else { "finished" },
                        elapsed_s,
                        plural(self.rollback_successes, "folder", "folders"),
                        plural(self.rollback_failures, "folder", "folders"),
                        plural(restored, "background file", "background files"),
                        kept_note,
                    );
                    let touched = std::mem::take(&mut self.rollback_touched_folders);
                    if !touched.is_empty() {
                        self.prune_scan_folders(&touched);
                    }
                    keep_rx = false;
                    if !touched.is_empty() && !self.is_scanning {
                        self.start_scan();
                    }
                    self.refresh_last_extras_job();
                }
            }
        }
        if keep_rx {
            if disconnected {
                self.extras_rolling_back = false;
                self.status = "Rollback worker disconnected".to_owned();
            } else {
                self.rollback_rx = Some(rx);
            }
        }
    }

    fn refresh_last_extras_job(&mut self) {
        self.extras_last_job = read_last_extras_job(&self.osu_root());
        self.extras_last_job_loaded = true;
    }

    /// Restore one session backup over its set folder, then rescan it.
    fn restore_shrink_backup(&mut self, index: usize) {
        if self.is_shrinking || self.is_analyzing {
            self.status = "Wait for the shrink run to finish before restoring".to_owned();
            return;
        }
        let Some(record) = self.shrink_backups.get(index).cloned() else {
            return;
        };
        match shrink::restore_backup(&record.zip, &record.folder) {
            Ok(count) => {
                self.status = format!(
                    "Restored {} to {}",
                    plural(count, "file", "files"),
                    record.folder.display()
                );
                let folder = record.folder.clone();
                let mut touched = BTreeSet::new();
                touched.insert(folder.clone());
                self.prune_scan_folders(&touched);
                if !self.is_scanning {
                    self.start_scan();
                }
                // Analysis is now stale for the restored folder.
                self.shrink_reports.retain(|r| r.folder != folder);
            }
            Err(err) => {
                self.status = format!("Restore failed: {err:#}");
            }
        }
    }

    fn start_update_check(&mut self) {
        if self.is_checking_updates || self.is_updating {
            self.status = "An update check or update is already running".to_owned();
            return;
        }

        let Some(scan) = &self.scan else {
            self.status = "Scan your Songs directory before checking for updates".to_owned();
            return;
        };

        let (targets, uncheckable) = updates::build_check_targets(&scan.maps);
        if targets.is_empty() {
            self.status = "No beatmapsets with online ids found to check".to_owned();
            return;
        }

        let (tx, rx) = mpsc::channel();
        self.update_check_rx = Some(rx);
        let pause = Arc::new(AtomicBool::new(false));
        self.update_check_pause = Some(pause.clone());
        let cancel = Arc::new(AtomicBool::new(false));
        self.update_check_cancel = Some(cancel.clone());
        self.update_check_db_note = None;
        self.update_log.clear();
        self.status = format!(
            "Starting update check for {}",
            plural(targets.len(), "beatmapset", "beatmapsets")
        );
        let backend_url = osu_oauth::backend_url();
        let osu_root = self.osu_root();
        let oauth_session = self.oauth_session.clone();
        std::thread::spawn(move || {
            run_update_check(
                targets,
                uncheckable,
                backend_url,
                osu_root,
                oauth_session,
                tx,
                pause,
                cancel,
            );
        });
    }

    fn update_job_for_set(&self, beatmapset_id: i64) -> Option<updates::UpdateJob> {
        self.outdated_sets
            .iter()
            .find(|set| set.beatmapset_id == beatmapset_id)
            .map(|set| updates::UpdateJob {
                beatmapset_id,
                folders: set.folders.clone(),
            })
    }

    fn start_update_all(&mut self) {
        if self.is_updating || self.is_checking_updates {
            self.status = "An update check or update is already running".to_owned();
            return;
        }
        if self.outdated_sets.is_empty() {
            self.status = "Check for updates first".to_owned();
            return;
        }
        let jobs = self
            .outdated_sets
            .iter()
            .filter_map(|set| self.update_job_for_set(set.beatmapset_id))
            .collect::<Vec<_>>();
        self.spawn_update_jobs(jobs);
    }

    fn start_update_single(&mut self, beatmapset_id: i64) {
        if self.is_updating || self.is_checking_updates {
            self.status = "An update check or update is already running".to_owned();
            return;
        }
        let Some(job) = self.update_job_for_set(beatmapset_id) else {
            self.status = format!("No pending update found for set {beatmapset_id}");
            return;
        };
        self.spawn_update_jobs(vec![job]);
    }

    fn spawn_update_jobs(&mut self, jobs: Vec<updates::UpdateJob>) {
        if jobs.is_empty() {
            self.status = "Nothing to update".to_owned();
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.update_rx = Some(rx);
        let pause = Arc::new(AtomicBool::new(false));
        self.update_pause = Some(pause.clone());
        let cancel = Arc::new(AtomicBool::new(false));
        self.update_cancel = Some(cancel.clone());
        self.status = format!(
            "Starting update for {}",
            plural(jobs.len(), "beatmapset", "beatmapsets")
        );
        let backend_url = osu_oauth::backend_url();
        let osu_root = self.osu_root();
        let oauth_session = self.oauth_session.clone();
        std::thread::spawn(move || {
            run_update_jobs(
                jobs,
                backend_url,
                osu_root,
                oauth_session,
                tx,
                pause,
                cancel,
            );
        });
    }

    /// Manual app update: ask GitHub for the latest release (user-triggered).
    fn start_app_update_check(&mut self) {
        if self.app_update_checking || self.app_update_downloading || self.app_update_installing {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.app_update_rx = Some(rx);
        self.app_update_checking = true;
        self.app_update_status = "Checking for app updates…".to_owned();
        self.app_update_error = None;
        self.app_update_up_to_date = false;
        spawn_app_update_check_worker(tx);
    }

    /// One-shot passive check at startup: when a newer release exists, the
    /// update button gets highlighted. Fails silently — this is a highlight,
    /// not a notification, so network errors must not disturb the user.
    fn start_app_update_background_check(&mut self) {
        if self.app_update_checking || self.app_update_downloading || self.app_update_installing {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.app_update_rx = Some(rx);
        self.app_update_checking = true;
        self.app_update_background_check = true;
        spawn_app_update_check_worker(tx);
    }

    /// Manual app update: download the release, swap the exe, restart.
    fn start_app_update_download(&mut self) {
        let Some(release) = self.app_update_release.clone() else {
            return;
        };
        if self.app_update_downloading || self.app_update_installing {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.app_update_rx = Some(rx);
        self.app_update_downloading = true;
        self.app_update_downloaded = 0;
        self.app_update_total = None;
        self.app_update_status = format!("Downloading {}…", release.asset_name);
        self.app_update_error = None;
        std::thread::spawn(move || {
            let progress_tx = tx.clone();
            let download = app_update::download_release_asset(&release, &|done, total| {
                let _ = progress_tx.send(AppUpdateEvent::DownloadProgress { done, total });
            });
            let staged = match download {
                Ok(staged) => staged,
                Err(err) => {
                    let _ = tx.send(AppUpdateEvent::Failed {
                        message: format!("{err:#}"),
                    });
                    return;
                }
            };
            let fresh = match app_update::extract_fresh_exe(&staged, &release.asset_name) {
                Ok(fresh) => fresh,
                Err(err) => {
                    let _ = tx.send(AppUpdateEvent::Failed {
                        message: format!("{err:#}"),
                    });
                    return;
                }
            };
            let _ = tx.send(AppUpdateEvent::DownloadFinished);
            if let Err(err) = app_update::install_and_restart(&fresh) {
                let _ = tx.send(AppUpdateEvent::Failed {
                    message: format!("{err:#}"),
                });
            }
            // On success `install_and_restart` exits the process, so no
            // further event is needed.
        });
    }

    fn poll_app_update(&mut self) {
        let Some(rx) = self.app_update_rx.take() else {
            return;
        };
        let mut keep_rx = true;
        loop {
            let event = match rx.try_recv() {
                Ok(event) => event,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    if self.app_update_checking
                        || self.app_update_downloading
                        || self.app_update_installing
                    {
                        self.app_update_checking = false;
                        self.app_update_downloading = false;
                        self.app_update_installing = false;
                        self.app_update_error = Some("Update worker stopped".to_owned());
                        self.app_update_status = "App update failed".to_owned();
                    }
                    keep_rx = false;
                    break;
                }
            };
            match event {
                AppUpdateEvent::CheckResult(Ok(None)) => {
                    self.app_update_checking = false;
                    self.app_update_release = None;
                    self.app_update_up_to_date = true;
                    self.app_update_status = format!(
                        "{} is the latest version",
                        app_update::current_version_text()
                    );
                    if !self.app_update_background_check {
                        self.status = self.app_update_status.clone();
                    }
                }
                AppUpdateEvent::CheckResult(Ok(Some(release))) => {
                    self.app_update_checking = false;
                    self.app_update_release = Some(release.clone());
                    self.app_update_up_to_date = false;
                    self.app_update_status = format!(
                        "Update available: {} → {}",
                        app_update::current_version_text(),
                        release.tag
                    );
                    if !self.app_update_background_check {
                        self.status = self.app_update_status.clone();
                    }
                }
                AppUpdateEvent::CheckResult(Err(message)) => {
                    self.app_update_checking = false;
                    if self.app_update_background_check {
                        // Passive startup check: a failure just means no
                        // highlight; keep it invisible.
                    } else {
                        self.app_update_error = Some(message.clone());
                        self.app_update_status = format!("App update check failed: {message}");
                        self.status = self.app_update_status.clone();
                    }
                }
                AppUpdateEvent::DownloadProgress { done, total } => {
                    self.app_update_downloaded = done;
                    self.app_update_total = total;
                    self.app_update_status = match total {
                        Some(total) if total > 0 => format!(
                            "Downloading… {:.1} / {:.1} MiB",
                            done as f64 / 1_048_576.0,
                            total as f64 / 1_048_576.0
                        ),
                        _ => format!("Downloading… {:.1} MiB", done as f64 / 1_048_576.0),
                    };
                }
                AppUpdateEvent::DownloadFinished => {
                    self.app_update_downloading = false;
                    self.app_update_installing = true;
                    self.app_update_status = "Installing… restarting the app".to_owned();
                    self.status = self.app_update_status.clone();
                }
                AppUpdateEvent::Failed { message } => {
                    self.app_update_checking = false;
                    self.app_update_downloading = false;
                    self.app_update_installing = false;
                    self.app_update_error = Some(message.clone());
                    self.app_update_status = format!("App update failed: {message}");
                    self.status = self.app_update_status.clone();
                }
            }
        }
        if keep_rx {
            self.app_update_rx = Some(rx);
        }
        if !self.app_update_checking {
            self.app_update_background_check = false;
        }
    }

    /// Drops every scan entry under the given folders so the next scan
    /// reparses the updated files instead of reusing stale cached data.
    fn prune_scan_folders(&mut self, folders: &BTreeSet<PathBuf>) {
        let Some(scan) = self.scan.as_mut() else {
            return;
        };
        scan.maps.retain(|map| !folders.contains(&map.folder));
        scan.file_meta
            .retain(|path, _| path.parent().is_none_or(|parent| !folders.contains(parent)));
        scan.problems.retain(|issue| {
            issue
                .beatmap
                .parent()
                .is_none_or(|parent| !folders.contains(parent))
        });
        scan.sets = build_sets_for_scan(&scan.maps);
        let expanded_gone = self
            .expanded_map_md5
            .as_ref()
            .is_some_and(|md5| !scan.maps.iter().any(|map| &map.md5 == md5));

        self.retain_selected_maps(|map| !folders.contains(&map.folder));
        if expanded_gone {
            self.expanded_map_md5 = None;
            self.clear_background_preview();
        }
        self.invalidate_scan_caches();
    }

    fn start_oauth_login(&mut self) {
        if self.is_signing_in {
            self.status = "osu! sign-in is already in progress".to_owned();
            return;
        }
        let backend_url = osu_oauth::backend_url();
        let backend = match osu_oauth::validated_backend_base_url(&backend_url) {
            Ok(backend) => backend,
            Err(err) => {
                self.oauth_status = format!("osu! sign-in failed: {err:#}");
                self.status = self.oauth_status.clone();
                return;
            }
        };
        let state = osu_oauth::generate_state();
        let code_verifier = osu_oauth::generate_code_verifier();
        self.oauth_pending_url = Some(osu_oauth::authorize_url(
            &backend,
            &state,
            &osu_oauth::code_challenge_s256(&code_verifier),
        ));
        let (tx, rx) = mpsc::channel();
        self.oauth_rx = Some(rx);
        self.is_signing_in = true;
        self.oauth_status = "Waiting for osu! sign-in in your browser...".to_owned();
        self.status = self.oauth_status.clone();
        std::thread::spawn(move || {
            let client = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(30))
                .build();
            let result = match client {
                Ok(client) => osu_oauth::login_with_state_blocking(
                    &client,
                    &backend_url,
                    &state,
                    &code_verifier,
                ),
                Err(err) => Err(err.into()),
            };
            let _ = tx.send(result);
        });
    }

    fn sign_out(&mut self) {
        self.oauth_session = None;
        self.oauth_pending_url = None;
        osu_oauth::clear_oauth_session(&self.osu_root());
        self.oauth_status = "Not signed in with osu! (downloads use the mirror)".to_owned();
        self.status = self.oauth_status.clone();
    }

    fn upsert_repair_log(&mut self, beatmapset_id: i64, status: RepairLogStatus, message: String) {
        upsert_log(&mut self.repair_log, beatmapset_id, status, message);
    }

    fn add_repair_ignores(&mut self, entries: Vec<IgnoredRepairIssue>) -> usize {
        let mut added = 0;
        for entry in entries {
            if !self.repair_ignores.entries.contains(&entry) {
                self.repair_ignores.entries.push(entry);
                added += 1;
            }
        }
        if added > 0
            && let Err(err) = save_repair_ignores(&self.osu_root(), &self.repair_ignores)
        {
            self.status = format!("Repair ignore save failed: {err:#}");
        }
        added
    }

    fn invalidate_scan_caches(&mut self) {
        self.scan_generation += 1;
        self.filtered_cache_key.clear();
        self.filtered_map_indexes.clear();
        self.filtered_pos_by_map_index.clear();
        self.repair_jobs_cache_key.clear();
        self.repair_jobs_cache.clear();
        // The md5 index and the collection-contents cache key off the
        // generation/map count, so they go stale automatically.
        self.collection_contents_key = (None, u64::MAX);
    }

    /// md5 → index inside the scan's map list, rebuilding the index when the
    /// scan changed. Keeps per-frame inspector/prefetch lookups O(1).
    fn map_index_for_md5(&mut self, md5: &str) -> Option<usize> {
        let key = (
            self.scan_generation,
            self.scan.as_ref().map_or(0, |scan| scan.maps.len()),
        );
        if self.md5_index_key != key {
            self.md5_to_map_index.clear();
            if let Some(scan) = &self.scan {
                self.md5_to_map_index.reserve(scan.maps.len());
                for (index, map) in scan.maps.iter().enumerate() {
                    self.md5_to_map_index.insert(map.md5.clone(), index);
                }
            }
            self.md5_index_key = key;
        }
        self.md5_to_map_index.get(md5).copied()
    }

    /// Cached [`collection_backup_info`]: reads the backup file's metadata
    /// (cheap) and only re-parses `collection.db` when the file changed.
    /// Called every frame from several places, so the uncached version
    /// would do disk I/O plus a full parse at display refresh rates.
    fn cached_collection_backup_info(&mut self) -> Option<BackupInfo> {
        let db_path = self.collection_db_path();
        let backup = db_path.as_ref()?.with_extension("db.bak");
        let fingerprint = fs::metadata(&backup).ok().and_then(|meta| {
            meta.modified()
                .ok()
                .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|age| (meta.len(), age.as_secs(), age.subsec_nanos()))
        });
        if self.backup_info_cache_path.as_ref() == Some(&backup)
            && self.backup_info_cache_file == fingerprint
        {
            return self.backup_info_cached.clone();
        }
        let info = collection_backup_info(db_path);
        self.backup_info_cache_path = Some(backup);
        self.backup_info_cache_file = fingerprint;
        self.backup_info_cached = info.clone();
        info
    }

    /// Rebuilds the md5 → map lookup for the currently selected
    /// collection's contents, but only when the selection or the scan
    /// changed. Callers then read `collection_contents_cache` directly
    /// (no per-frame rebuild, no per-frame clone of the whole map).
    fn refresh_collection_contents(&mut self) {
        let key = (self.selected_collection_index, self.scan_generation);
        if self.collection_contents_key == key {
            return;
        }
        self.collection_contents_cache.clear();
        if let (Some(index), Some(scan)) = (self.selected_collection_index, &self.scan)
            && let Some(collection) = self.collections.get(index)
        {
            let wanted: BTreeSet<&str> = collection.hashes.iter().map(String::as_str).collect();
            for map in &scan.maps {
                if wanted.contains(map.md5.as_str()) {
                    self.collection_contents_cache
                        .insert(map.md5.clone(), map.clone());
                }
            }
        }
        self.collection_contents_key = key;
    }

    fn filtered_cache_key(&self) -> String {
        // Generation (not just the map count) keys the cache, so any scan
        // mutation refreshes the list even when the count does not change.
        // The count is kept too so the list still grows live while scanning.
        let map_count = self.scan.as_ref().map_or(0, |scan| scan.maps.len());
        serde_json::to_string(&self.filters).unwrap_or_default()
            + "#"
            + &self.scan_generation.to_string()
            + "#"
            + &map_count.to_string()
    }

    fn refresh_filtered_maps(&mut self) {
        let key = self.filtered_cache_key();
        if key == self.filtered_cache_key {
            return;
        }

        self.filtered_map_indexes.clear();
        self.filtered_pos_by_map_index.clear();
        if let Some(scan) = &self.scan {
            let text = self.filters.lowered_text();
            self.filtered_map_indexes
                .extend(scan.maps.iter().enumerate().filter_map(|(index, map)| {
                    self.filters
                        .matches_local_lowered(map, &text)
                        .then_some(index)
                }));
            self.filtered_pos_by_map_index
                .reserve(self.filtered_map_indexes.len());
            for (position, &map_index) in self.filtered_map_indexes.iter().enumerate() {
                self.filtered_pos_by_map_index.insert(map_index, position);
            }
        }
        self.filtered_cache_key = key;
    }

    fn repair_jobs_cache_key(&self) -> String {
        let Some(scan) = &self.scan else {
            return String::new();
        };
        // The opt-out flag is part of the key so toggling the checkbox
        // regroups the jobs immediately, without a rescan.
        format!(
            "{}:{}:{}:{}",
            self.scan_generation,
            scan.maps.len(),
            scan.problems.len(),
            self.repair_ignores.ignore_missing_backgrounds
        )
    }

    /// Rebuilds the repair-jobs cache when the scan changed. While a scan is
    /// streaming, rebuilds are rate-limited to [`REPAIR_JOBS_REBUILD_INTERVAL`]:
    /// with many flagged maps (e.g. deleted backgrounds) the key changes on
    /// every streamed map event, and an O(library) rebuild per frame froze the
    /// UI for the whole scan.
    fn refresh_repair_jobs(&mut self) {
        self.refresh_repair_jobs_limited(false);
    }

    /// Unthrottled rebuild for user-initiated actions that must act on the
    /// current problems (repair all / repair single), even mid-scan.
    fn refresh_repair_jobs_now(&mut self) {
        self.refresh_repair_jobs_limited(true);
    }

    fn refresh_repair_jobs_limited(&mut self, force: bool) {
        let key = self.repair_jobs_cache_key();
        if key == self.repair_jobs_cache_key {
            return;
        }
        if !force
            && self.is_scanning
            && self.repair_jobs_last_rebuild.elapsed() < REPAIR_JOBS_REBUILD_INTERVAL
        {
            return;
        }
        let ignore_missing_backgrounds = self.repair_ignores.ignore_missing_backgrounds;
        self.repair_jobs_cache = self
            .scan
            .as_ref()
            .map(|scan| repair_jobs(scan, ignore_missing_backgrounds))
            .unwrap_or_default();
        self.repair_jobs_cache_key = key;
        self.repair_jobs_last_rebuild = Instant::now();
    }

    fn clear_selection(&mut self) {
        self.selected_maps.clear();
        self.selected_md5s.clear();
    }

    fn select_map(&mut self, map: &LocalBeatmap) {
        if self.selected_md5s.insert(map.md5.clone()) {
            self.selected_maps.push(map.clone());
        }
    }

    fn deselect_md5(&mut self, md5: &str) {
        self.selected_md5s.remove(md5);
        self.selected_maps.retain(|selected| selected.md5 != md5);
    }

    fn select_missing_hash(&mut self, md5: &str) {
        self.selected_md5s.insert(md5.to_owned());
    }

    fn retain_selected_maps(&mut self, mut keep: impl FnMut(&LocalBeatmap) -> bool) {
        self.selected_maps.retain(|map| keep(map));
        self.selected_md5s = reconcile_selected_md5s(
            &self.selected_maps,
            &self.collection_missing_hashes,
            &self.selected_md5s,
        );
    }

    fn map_result_label(&self, map: &LocalBeatmap) -> String {
        let mut fields = filter_label_values(&self.filters, map);

        if fields.is_empty()
            && let Some(stars) = map.stars
        {
            fields.push(format!("*{}", format_number(stars)));
        }

        let title = format!("{} - {}", map.artist, map.title);
        if fields.is_empty() {
            title
        } else {
            format!("{title} ({})", fields.join(", "))
        }
    }

    /// Results workspace: map list next to the inspector, laid out in normal
    /// egui flow (no manual rects), so nothing can slide above or below its
    /// frame. Both panes scroll internally within the same fixed height.
    fn render_map_workspace(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, max_height: f32) {
        let height = max_height.max(120.0);
        let gap = ui.spacing().item_spacing.x;
        let total_width = ui.available_width().max(1.0);
        let list_width = (total_width * 0.42).clamp(300.0, 430.0);
        let inspector_width = (total_width - list_width - gap).max(220.0);

        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(list_width, height),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    egui::Frame::none()
                        .fill(egui::Color32::from_rgb(0x20, 0x21, 0x24))
                        .stroke(egui::Stroke::new(
                            1.0_f32,
                            egui::Color32::from_rgb(0x3a, 0x3b, 0x40),
                        ))
                        .show(ui, |ui| {
                            ui.set_min_height(height);
                            self.render_map_browser(ui, height - 4.0);
                        });
                },
            );
            ui.allocate_ui_with_layout(
                egui::vec2(inspector_width, height),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    let inspected_map = self.expanded_map_md5.clone().and_then(|md5| {
                        let index = self.map_index_for_md5(&md5)?;
                        self.scan.as_ref()?.maps.get(index).cloned()
                    });
                    if let Some(map) = inspected_map {
                        self.render_map_inspector(ui, ctx, height, &map);
                    } else {
                        inspector_frame(ctx.style().as_ref()).show(ui, |ui| {
                            fill_tile_width(ui);
                            ui.set_min_height(height - 4.0);
                            ui.centered_and_justified(|ui| {
                                muted_label(ui, "Select a map from the list to inspect it.");
                            });
                        });
                    }
                },
            );
        });
    }

    fn render_map_browser(&mut self, ui: &mut egui::Ui, max_height: f32) {
        const HEADER_HEIGHT: f32 = 46.0;

        let inspected_row = self.expanded_map_md5.clone().and_then(|expanded_md5| {
            let map_index = self.map_index_for_md5(&expanded_md5)?;
            self.filtered_pos_by_map_index.get(&map_index).copied()
        });
        let row_count = self.filtered_map_indexes.len();
        let total_height = row_count as f32 * HEADER_HEIGHT;

        egui::ScrollArea::vertical()
            .id_source("local_maps")
            .max_height(max_height)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let width = ui.available_width().max(1.0);
                let (list_rect, _) = ui.allocate_exact_size(
                    egui::vec2(width, total_height.max(1.0)),
                    egui::Sense::hover(),
                );
                let clip_rect = ui.clip_rect();

                // An empty result is a state to act on, not a blank void.
                if row_count == 0 {
                    ui.set_min_height(max_height.max(120.0) - 8.0);
                    ui.centered_and_justified(|ui| {
                        muted_label(ui, "No maps match your filters");
                        ui.add_space(4.0);
                        if ui.button("Clear filters").clicked() {
                            self.filters.clear_all();
                        }
                    });
                    return;
                }

                for row in 0..row_count {
                    let row_top = list_rect.top() + row as f32 * HEADER_HEIGHT;
                    let is_inspected = inspected_row == Some(row);
                    let row_rect = egui::Rect::from_min_size(
                        egui::pos2(list_rect.left(), row_top),
                        egui::vec2(width, HEADER_HEIGHT),
                    );
                    if !row_rect.intersects(clip_rect) {
                        continue;
                    }

                    // Copy out only the small display fields here; the full
                    // map is cloned lazily below, only when the user
                    // actually toggles this row's selection.
                    let Some((map_md5, title, details, label)) =
                        self.scan.as_ref().and_then(|scan| {
                            self.filtered_map_indexes
                                .get(row)
                                .and_then(|&index| scan.maps.get(index))
                                .map(|map| {
                                    (
                                        map.md5.clone(),
                                        format!("{} - {}", map.artist, map.title),
                                        compact_map_details(map),
                                        map.label(),
                                    )
                                })
                        })
                    else {
                        continue;
                    };

                    let header_rect =
                        egui::Rect::from_min_size(row_rect.min, egui::vec2(width, HEADER_HEIGHT));
                    let selected = self.selected_md5s.contains(&map_md5);
                    let fill = if is_inspected {
                        egui::Color32::from_rgb(0x30, 0x2e, 0x2a)
                    } else if selected {
                        egui::Color32::from_rgb(0x35, 0x2c, 0x33)
                    } else {
                        egui::Color32::from_rgb(0x23, 0x24, 0x27)
                    };
                    ui.painter().rect_filled(header_rect, 0.0, fill);
                    if selected {
                        // Accent rail: a checkbox tick alone is too faint a
                        // selection signal on dark rows.
                        ui.painter().rect_filled(
                            egui::Rect::from_min_size(
                                header_rect.min,
                                egui::vec2(3.0, HEADER_HEIGHT),
                            ),
                            0.0,
                            egui::Color32::from_rgb(0xd8, 0x9a, 0xb0),
                        );
                    }
                    ui.painter().line_segment(
                        [header_rect.left_bottom(), header_rect.right_bottom()],
                        egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(0x36, 0x37, 0x3b)),
                    );

                    let mut selected_value = selected;
                    let checkbox_column_width = 38.0;
                    let checkbox_area = egui::Rect::from_min_size(
                        egui::pos2(header_rect.left(), header_rect.top()),
                        egui::vec2(checkbox_column_width, HEADER_HEIGHT),
                    );
                    // Stable per-map id scope: virtualized rows enter/leave the
                    // clip rect while scrolling, so positional auto-ids would
                    // shift between frames and clash.
                    let selection_changed = ui
                        .push_id(("map_select", &map_md5), |ui| {
                            ui.allocate_ui_at_rect(checkbox_area, |ui| {
                                ui.centered_and_justified(|ui| ui.checkbox(&mut selected_value, ""))
                                    .inner
                                    .changed()
                            })
                            .inner
                        })
                        .inner;
                    if selection_changed {
                        if selected_value {
                            if let Some(selected_map) = self.scan.as_ref().and_then(|scan| {
                                self.filtered_map_indexes
                                    .get(row)
                                    .and_then(|&index| scan.maps.get(index))
                                    .cloned()
                            }) {
                                self.select_map(&selected_map);
                            }
                        } else {
                            self.deselect_md5(&map_md5);
                        }
                    }

                    let content_rect = egui::Rect::from_min_max(
                        egui::pos2(
                            header_rect.left() + checkbox_column_width,
                            header_rect.top(),
                        ),
                        header_rect.right_bottom(),
                    );
                    let row_response = ui.interact(
                        content_rect,
                        ui.id().with(("map_inspector", &map_md5)),
                        egui::Sense::click(),
                    );
                    let title_rect = egui::Rect::from_min_max(
                        egui::pos2(content_rect.left() + 10.0, content_rect.top() + 4.0),
                        egui::pos2(content_rect.right() - 8.0, content_rect.top() + 23.0),
                    );
                    let details_rect = egui::Rect::from_min_max(
                        egui::pos2(content_rect.left() + 10.0, content_rect.top() + 23.0),
                        egui::pos2(content_rect.right() - 8.0, content_rect.bottom() - 3.0),
                    );
                    ui.painter().with_clip_rect(title_rect).text(
                        title_rect.left_center(),
                        egui::Align2::LEFT_CENTER,
                        title.clone(),
                        egui::TextStyle::Body.resolve(ui.style()),
                        ui.visuals().text_color(),
                    );
                    ui.painter().with_clip_rect(details_rect).text(
                        details_rect.left_center(),
                        egui::Align2::LEFT_CENTER,
                        details.clone(),
                        egui::TextStyle::Small.resolve(ui.style()),
                        egui::Color32::from_rgb(0xb3, 0xad, 0xa5),
                    );
                    let row_response = row_response.on_hover_text(format!("{label}\n{details}"));
                    if row_response.clicked() {
                        self.expanded_map_md5 = Some(map_md5);
                    }
                }
            });
    }

    fn render_map_inspector(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        max_height: f32,
        map: &LocalBeatmap,
    ) {
        let background_path = map
            .background_filename
            .as_ref()
            .map(|filename| map.folder.join(filename));
        if let Some(path) = background_path.as_deref() {
            self.ensure_background_preview(path, &map.md5);
        } else {
            self.clear_background_preview();
        }
        let preview = self.background_preview.clone();
        let preview_error = self.background_preview_error.clone();

        inspector_frame(ctx.style().as_ref()).show(ui, |ui| {
            fill_tile_width(ui);
            // Scrolls internally within the allotted height so tall details
            // never slide under the bottom status bar.
            egui::ScrollArea::vertical()
                .id_source(("inspector_scroll", &map.md5))
                .max_height(max_height.max(1.0))
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    fill_tile_width(ui);
                    if let Some(texture) = preview {
                        // Fixed box, filled edge-to-edge (cover): every background
                        // shows at exactly the same size no matter its resolution.
                        let source_size = texture.size_vec2();
                        let box_size =
                            egui::vec2(ui.available_width().max(1.0), MAP_PREVIEW_HEIGHT);
                        let (box_rect, _) = ui.allocate_exact_size(box_size, egui::Sense::hover());
                        if source_size.x > 0.0 && source_size.y > 0.0 {
                            let scale = (box_rect.width() / source_size.x)
                                .max(box_rect.height() / source_size.y);
                            let shown_rect = egui::Rect::from_center_size(
                                box_rect.center(),
                                source_size * scale,
                            );
                            let previous_clip = ui.clip_rect();
                            ui.set_clip_rect(box_rect.intersect(previous_clip));
                            ui.painter().image(
                                texture.id(),
                                shown_rect,
                                egui::Rect::from_min_max(
                                    egui::Pos2::ZERO,
                                    egui::Pos2::new(1.0, 1.0),
                                ),
                                egui::Color32::WHITE,
                            );
                            ui.set_clip_rect(previous_clip);
                        }
                    } else {
                        let message = preview_error.unwrap_or_else(|| {
                            if background_path.is_some() {
                                "Background file is unavailable".to_owned()
                            } else {
                                "This map does not define a background".to_owned()
                            }
                        });
                        // Same box as a real background, so the list never jumps.
                        let box_size =
                            egui::vec2(ui.available_width().max(1.0), MAP_PREVIEW_HEIGHT);
                        let (box_rect, _) = ui.allocate_exact_size(box_size, egui::Sense::hover());
                        ui.painter().rect_filled(
                            box_rect,
                            egui::Rounding::ZERO,
                            egui::Color32::from_rgb(0x1b, 0x1c, 0x1f),
                        );
                        ui.painter().rect_stroke(
                            box_rect,
                            egui::Rounding::ZERO,
                            egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(0x3a, 0x3b, 0x40)),
                        );
                        ui.allocate_ui_at_rect(box_rect, |ui| {
                            ui.centered_and_justified(|ui| muted_label(ui, message));
                        });
                    }

                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(format!("{} - {}", map.artist, map.title))
                            .size(18.0)
                            .strong(),
                    );
                    muted_label(ui, format!("{} · mapped by {}", map.version, map.creator));

                    let audio_path = map
                        .audio_filename
                        .as_ref()
                        .map(|filename| map.folder.join(filename));
                    let audio_available = audio_path.as_ref().is_some_and(|path| path.exists());
                    let is_current_audio = audio_path.as_ref().is_some_and(|path| {
                        self.audio_player
                            .as_ref()
                            .is_some_and(|player| player.path == *path)
                    });
                    ui.horizontal_wrapped(|ui| {
                        if is_current_audio {
                            let paused = self
                                .audio_player
                                .as_ref()
                                .is_some_and(|player| player.sink.is_paused());
                            if ui.button(if paused { "Resume" } else { "Pause" }).clicked() {
                                self.toggle_audio_pause();
                            }
                            if ui.button("Stop").clicked() {
                                self.stop_audio_playback();
                            }
                        } else if ui
                            .add_enabled(audio_available, egui::Button::new("Play audio"))
                            .clicked()
                            && let Some(path) = audio_path.as_deref()
                        {
                            self.start_audio_playback(path);
                        }
                        if ui.button("Open map folder").clicked() {
                            self.open_map_path(&map.folder, "map folder");
                        }
                    });
                    if is_current_audio {
                        ui.horizontal(|ui| {
                            ui.label("Volume");
                            let changed = ui
                                .add(
                                    egui::Slider::new(&mut self.audio_volume, 0.0..=1.0)
                                        .show_value(false),
                                )
                                .changed();
                            if changed && let Some(player) = &self.audio_player {
                                player.sink.set_volume(self.audio_volume);
                            }
                            if let Some(filename) = &map.audio_filename {
                                muted_label(ui, format!("FFmpeg | {filename}"));
                            }
                        });
                    } else if let Some(filename) = &map.audio_filename {
                        muted_label(ui, filename);
                    }

                    if !audio_available {
                        if map.audio_filename.is_some() {
                            muted_label(ui, "The referenced audio file is missing.");
                        } else {
                            muted_label(ui, "This map does not define an audio file.");
                        }
                    }

                    ui.separator();
                    egui::Grid::new(("map_info", &map.md5))
                        .num_columns(4)
                        .spacing(egui::vec2(14.0, 6.0))
                        .show(ui, |ui| {
                            inspector_grid_value(ui, "Mode", mode_label(map.mode));
                            inspector_grid_value(
                                ui,
                                "Length",
                                map.length_seconds
                                    .map(format_duration)
                                    .unwrap_or_else(|| "—".to_owned()),
                            );
                            ui.end_row();
                            inspector_grid_value(
                                ui,
                                "Stars",
                                map.stars
                                    .map(format_number)
                                    .unwrap_or_else(|| "—".to_owned()),
                            );
                            inspector_grid_value(
                                ui,
                                "BPM",
                                map.bpm.map(format_number).unwrap_or_else(|| "—".to_owned()),
                            );
                            ui.end_row();
                            inspector_grid_value(ui, "AR", optional_number(map.ar));
                            inspector_grid_value(ui, "CS", optional_number(map.cs));
                            ui.end_row();
                            inspector_grid_value(ui, "OD", optional_number(map.od));
                            inspector_grid_value(ui, "HP", optional_number(map.hp));
                            ui.end_row();
                            inspector_grid_value(ui, "Circles", map.circles.to_string());
                            inspector_grid_value(ui, "Sliders", map.sliders.to_string());
                            ui.end_row();
                            inspector_grid_value(
                                ui,
                                "Map ID",
                                map.beatmap_id
                                    .map_or_else(|| "—".to_owned(), |id| id.to_string()),
                            );
                            inspector_grid_value(
                                ui,
                                "Set ID",
                                map.beatmapset_id
                                    .map_or_else(|| "—".to_owned(), |id| id.to_string()),
                            );
                            ui.end_row();
                        });
                });
        });
    }

    fn render_shrink_page(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let content_width = ui.available_width().max(1.0);
        egui::ScrollArea::vertical()
            .id_source("shrink_pane")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let card_item_spacing = ui.spacing().item_spacing;
                let card_gap = 8.0;
                ui.spacing_mut().item_spacing.y = 0.0;
                fix_ui_width(ui, content_width);

                if self.scan.is_none() {
                    section_frame(ctx.style().as_ref()).show(ui, |ui| {
                        ui.spacing_mut().item_spacing = card_item_spacing;
                        fill_tile_width(ui);
                        ui.heading("No scan yet");
                        muted_label(
                            ui,
                            "Enter your Songs folder in the sidebar, then press Load my maps.",
                        );
                    });
                    return;
                }

                // ── 1. Find ──
                section_frame(ctx.style().as_ref()).show(ui, |ui| {
                    ui.spacing_mut().item_spacing = card_item_spacing;
                    fill_tile_width(ui);
                    ui.heading("🗜 Find files to shrink");
                    muted_label(
                        ui,
                        "Finds shrinkable song audio, background video and images. \
                        Filenames and .osu files are never changed, so scores stay \
                        submittable. Already-shrunk files are remembered and skipped. \
                        Close osu! before shrinking.",
                    );
                    ui.add_space(4.0);
                    ui.horizontal_wrapped(|ui| {
                        if self.is_analyzing {
                            if ui.button("⏹ Stop").clicked() {
                                self.stop_shrink_analysis();
                            }
                            let paused = self.analysis_paused();
                            if ui
                                .button(if paused { "▶ Resume" } else { "⏸ Pause" })
                                .on_hover_text(
                                    "Pause takes effect after the current folder",
                                )
                                .clicked()
                            {
                                self.toggle_analysis_pause();
                            }
                            ui.add(egui::Spinner::new());
                            muted_label(
                                ui,
                                if paused {
                                    "Paused — resume to continue".to_owned()
                                } else {
                                    format!(
                                        "Checking {}/{}…",
                                        self.analysis_done,
                                        plural(self.analysis_total, "set", "sets")
                                    )
                                },
                            );
                        } else {
                            if ui
                                .add_enabled(
                                    !self.is_shrinking,
                                    egui::Button::new("🔍 Check library"),
                                )
                                .on_hover_text("Find which files can be made smaller")
                                .clicked()
                            {
                                self.start_shrink_analysis();
                            }
                            if ui
                                .add_enabled(
                                    !self.is_shrinking,
                                    egui::Button::new("🎨 Check skins"),
                                )
                                .on_hover_text(
                                    "Lossless-ish image pass over <osu root>/Skins: same pixels, \
                                    same names, tighter encodes. skin.ini and sounds untouched.",
                                )
                                .clicked()
                            {
                                self.start_skin_analysis();
                            }
                            if ui
                                .small_button("Re-check everything")
                                .on_hover_text(
                                    "Forget which files were already shrunk, so the next \
                                    check plans everything again.",
                                )
                                .clicked()
                            {
                                let path = shrink_cache_path(&self.osu_root());
                                match std::fs::remove_file(&path) {
                                    Ok(()) => {
                                        self.status =
                                            "Cleared — the next check plans everything again"
                                                .to_owned()
                                    }
                                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                                        self.status =
                                            "Nothing to re-check — everything is already planned"
                                                .to_owned()
                                    }
                                    Err(err) => {
                                        self.status =
                                            format!("Could not reset the shrink list: {err}")
                                    }
                                }
                            }
                        }
                        ui.checkbox(&mut self.shrink_options.backup, "Back up each set (.zip)")
                            .on_hover_text(
                                "Zips the set folder before touching it. Restore from the Backups section.",
                            );
                    });
                    muted_label(
                        ui,
                        "Guarantee: filenames never change and .osu/.osb files are never \
                        written, so maps keep submitting scores.",
                    );
                    ui.horizontal_wrapped(|ui| {
                        let mut orphans = self.shrink_options.delete_orphans;
                        if ui
                            .checkbox(&mut orphans, "Delete unused files")
                            .on_hover_text(
                                "Also delete media files nothing references. Off by default; \
                                re-checks when changed.",
                            )
                            .changed()
                        {
                            self.shrink_options.delete_orphans = orphans;
                            if !self.shrink_reports.is_empty()
                                && !self.is_analyzing
                                && !self.is_shrinking
                            {
                                self.start_shrink_analysis();
                            }
                        }
                        let mut remove_videos = self.shrink_options.remove_videos;
                        if ui
                            .checkbox(&mut remove_videos, "Remove background videos")
                            .on_hover_text(
                                "Delete referenced background videos instead of compressing \
                                them. The game shows the background image instead, and \
                                .osu files are untouched. Off by default; re-checks \
                                when changed. Backups recommended.",
                            )
                            .changed()
                        {
                            self.shrink_options.remove_videos = remove_videos;
                            if !self.shrink_reports.is_empty()
                                && !self.is_analyzing
                                && !self.is_shrinking
                            {
                                self.start_shrink_analysis();
                            }
                        }
                    });
                    // A CPU-scheduling knob has no business on the main card;
                    // the default (2) is right for almost everyone.
                    egui::CollapsingHeader::new("⚙ Advanced")
                        .default_open(false)
                        .show(ui, |ui| {
                            ui.add(
                                egui::DragValue::new(&mut self.shrink_options.jobs)
                                    .clamp_range(1..=4)
                                    .prefix("Parallel sets: "),
                            )
                            .on_hover_text(
                                "How many set folders to shrink at once. x264 is CPU-heavy; \
                                2 suits most machines, 1 is the most disk-friendly.",
                            );
                        });
                    if !self.shrink_reports.is_empty() {
                        let (total_in, total_est, items) =
                            shrink::summarize(&self.shrink_reports);
                        let cached: usize =
                            self.shrink_reports.iter().map(|r| r.cached_items()).sum();
                        muted_label(
                            ui,
                            format!(
                                "{} · {} shrinkable{} · {} → ~{} (saves ~{})",
                                plural(self.shrink_reports.len(), "set", "sets"),
                                plural(items, "file", "files"),
                                if cached > 0 {
                                    format!(" · {} already done", cached)
                                } else {
                                    String::new()
                                },
                                shrink::human_bytes(total_in),
                                shrink::human_bytes(total_est),
                                shrink::human_bytes(total_in.saturating_sub(total_est)),
                            ),
                        );
                    }
                });
                ui.add_space(card_gap);

                // ── 2. Run ──
                section_frame(ctx.style().as_ref()).show(ui, |ui| {
                    ui.spacing_mut().item_spacing = card_item_spacing;
                    fill_tile_width(ui);
                    ui.heading("Shrink");
                    let ready = self
                        .shrink_reports
                        .iter()
                        .filter(|r| r.work_items() > 0)
                        .count();
                    ui.horizontal_wrapped(|ui| {
                        if self.is_shrinking {
                            if ui
                                .button("⏹ Stop")
                                .on_hover_text(
                                    "Finishes the current file, then stops; files already shrunk are kept",
                                )
                                .clicked()
                            {
                                self.stop_shrink();
                            }
                            let paused = self.shrink_paused();
                            if ui
                                .button(if paused { "▶ Resume" } else { "⏸ Pause" })
                                .on_hover_text(
                                    "Pause takes effect after the current file; \
                                    in-flight encodes always finish first",
                                )
                                .clicked()
                            {
                                self.toggle_shrink_pause();
                            }
                            ui.add(egui::Spinner::new());
                            muted_label(
                                ui,
                                if paused {
                                    "Paused — resume to continue".to_owned()
                                } else {
                                    format!(
                                        "{}/{} files · saved {}",
                                        self.shrink_done_assets,
                                        self.shrink_total_assets,
                                        shrink::human_bytes(self.shrink_saved_bytes),
                                    )
                                },
                            );
                        } else if ui
                            .add_enabled(
                                ready > 0,
                                egui::Button::new(format!("Shrink {}", plural(ready, "set", "sets"))),
                            )
                            .on_hover_text("Asks for confirmation, then compresses every analyzed set")
                            .clicked()
                        {
                            // Same scope as the background replacement: a
                            // whole-library write, so it confirms first.
                            self.delete_confirmation = Some(DeleteIntent::ShrinkRun { sets: ready });
                        }
                    });
                    if !self.shrink_log.is_empty() {
                        egui::ScrollArea::vertical()
                            .id_source("shrink_log")
                            .max_height(160.0)
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                for entry in self.shrink_log.iter().rev().take(40) {
                                    let dot = match entry.status {
                                        RepairLogStatus::Success => "✓",
                                        RepairLogStatus::Failed => "✗",
                                        RepairLogStatus::InProgress => "…",
                                        RepairLogStatus::Skipped => "–",
                                    };
                                    ui.label(format!(
                                        "{dot} {}: {}",
                                        entry.label, entry.message
                                    ));
                                }
                            });
                    }
                });
                ui.add_space(card_gap);

                // ── 3. Backups ──
                section_frame(ctx.style().as_ref()).show(ui, |ui| {
                    ui.spacing_mut().item_spacing = card_item_spacing;
                    fill_tile_width(ui);
                    ui.heading("Backups");
                    if self.shrink_backups.is_empty() {
                        muted_label(
                            ui,
                            "No backups this session. Each shrunk set is zipped before it is touched.",
                        );
                    } else {
                        for index in 0..self.shrink_backups.len() {
                            let (zip_name, folder_name) = {
                                let record = &self.shrink_backups[index];
                                (
                                    record
                                        .zip
                                        .file_name()
                                        .and_then(|n| n.to_str())
                                        .unwrap_or("backup.zip")
                                        .to_owned(),
                                    record
                                        .folder
                                        .file_name()
                                        .and_then(|n| n.to_str())
                                        .unwrap_or("set")
                                        .to_owned(),
                                )
                            };
                            ui.horizontal(|ui| {
                                ui.label(format!("{folder_name} ({zip_name})"));
                                if ui
                                    .small_button("Restore")
                                    .on_hover_text("Puts the backed-up folder back, replacing what is there now")
                                    .clicked()
                                {
                                    // Overwrites the whole set folder: confirm.
                                    self.delete_confirmation =
                                        Some(DeleteIntent::RestoringShrinkBackup {
                                            index,
                                            folder: folder_name.clone(),
                                        });
                                }
                            });
                        }
                    }
                });
            });
    }

    fn render_extras_page(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let content_width = ui.available_width().max(1.0);
        egui::ScrollArea::vertical()
            .id_source("extras_pane")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let card_item_spacing = ui.spacing().item_spacing;
                let card_gap = 8.0;
                ui.spacing_mut().item_spacing.y = 0.0;
                fix_ui_width(ui, content_width);

                if self.scan.is_none() {
                    section_frame(ctx.style().as_ref()).show(ui, |ui| {
                        ui.spacing_mut().item_spacing = card_item_spacing;
                        fill_tile_width(ui);
                        ui.heading("No scan yet");
                        muted_label(
                            ui,
                            "Enter your Songs folder in the sidebar, then press Load my maps.",
                        );
                    });
                    return;
                }

                // ── 1. Import ──
                section_frame(ctx.style().as_ref()).show(ui, |ui| {
                    ui.spacing_mut().item_spacing = card_item_spacing;
                    fill_tile_width(ui);
                    ui.heading("🖼 Background image");
                    muted_label(
                        ui,
                        "Pick an image, then apply it to every beatmapset in the scan. \
                        It is written over the background files the maps already use; \
                        the .osu/.osb chart files themselves are never altered.",
                    );
                    ui.add_space(4.0);
                    ui.horizontal_wrapped(|ui| {
                        if self.extras_image_rx.is_some() {
                            ui.add(egui::Spinner::new());
                            muted_label(ui, "Importing image…");
                        } else if ui
                            .add_enabled(
                                !self.extras_running,
                                egui::Button::new("📥 Choose image…"),
                            )
                            .on_hover_text(
                                "Pick a jpg, png, webp or bmp file to use as the new background",
                            )
                            .clicked()
                        {
                            self.pick_extras_image();
                        }
                        if let Some(name) = self.extras_image_name.as_deref() {
                            ui.separator();
                            ui.label(egui::RichText::new(name).strong());
                            if let Some((width, height)) = self.extras_image_size {
                                let bytes = self
                                    .extras_image_bytes
                                    .as_ref()
                                    .map_or(0, |bytes| bytes.len());
                                muted_label(
                                    ui,
                                    format!(
                                        "{width}×{height} · {}",
                                        shrink::human_bytes(bytes as u64)
                                    ),
                                );
                            }
                            if ui
                                .small_button("✕")
                                .on_hover_text("Forget the imported image")
                                .clicked()
                                && !self.extras_running
                            {
                                self.clear_extras_image();
                            }
                        }
                    });
                    if let Some(texture) = self.extras_image_texture.clone() {
                        // Contain-fit inside a fixed-height box so every
                        // image previews at a predictable size.
                        let source_size = texture.size_vec2();
                        let box_size =
                            egui::vec2(ui.available_width().max(1.0), EXTRAS_PREVIEW_HEIGHT);
                        let (box_rect, _) = ui.allocate_exact_size(box_size, egui::Sense::hover());
                        if source_size.x > 0.0 && source_size.y > 0.0 {
                            let scale = (box_rect.width() / source_size.x)
                                .min(box_rect.height() / source_size.y);
                            let shown_rect = egui::Rect::from_center_size(
                                box_rect.center(),
                                source_size * scale,
                            );
                            ui.painter().image(
                                texture.id(),
                                shown_rect,
                                egui::Rect::from_min_max(
                                    egui::Pos2::ZERO,
                                    egui::Pos2::new(1.0, 1.0),
                                ),
                                egui::Color32::WHITE,
                            );
                        }
                    }
                });
                ui.add_space(card_gap);

                // ── 2. Apply ──
                section_frame(ctx.style().as_ref()).show(ui, |ui| {
                    ui.spacing_mut().item_spacing = card_item_spacing;
                    fill_tile_width(ui);
                    ui.heading("🎨 Apply to every beatmap");
                    muted_label(
                        ui,
                        "Writes the image over the content of every background file \
                        the scanned charts reference, encoded to match each file's \
                        extension, so every difficulty shows it. Close osu! before applying.",
                    );
                    muted_label(
                        ui,
                        "Guarantee: .osu and .osb files are read but never written, so map \
                        checksums, local scores and score submission are unaffected. Maps \
                        without a background line keep their look (adding one would need \
                        chart edits), and storyboard art is never touched.",
                    );
                    ui.add_space(4.0);
                    let (sets, maps) = self
                        .scan
                        .as_ref()
                        .map(|scan| (scan.sets.len(), scan.maps.len()))
                        .unwrap_or((0, 0));
                    ui.horizontal_wrapped(|ui| {
                        if self.extras_running {
                            if ui
                                .button("⏹ Stop")
                                .on_hover_text("Finishes the current folder, then stops; folders already done are kept")
                                .clicked()
                            {
                                self.stop_extras();
                            }
                            ui.add(egui::Spinner::new());
                            muted_label(ui, self.extras_progress.clone());
                        } else if ui
                            .add_enabled(
                                self.extras_image_bytes.is_some(),
                                egui::Button::new(format!(
                                    "🎨 Set background on {}",
                                    plural(sets, "set", "sets")
                                )),
                            )
                            .on_hover_text(format!(
                                "Asks for confirmation, then replaces the content of every \
                                 referenced background image across {} / {} \
                                 without touching .osu/.osb files",
                                plural(sets, "set folder", "set folders"),
                                plural(maps, "map", "maps")
                            ))
                            .clicked()
                        {
                            // Not applied directly: a modal confirmation
                            // states the scope and undo first.
                            self.delete_confirmation = Some(DeleteIntent::SetBackground { sets });
                        }
                    });
                    if self.extras_running {
                        let progress = if self.extras_folders_total > 0 {
                            self.extras_folders_done as f32 / self.extras_folders_total as f32
                        } else {
                            0.0
                        };
                        ui.add(
                            egui::ProgressBar::new(progress)
                                .show_percentage()
                                .text(format!(
                                    "{}/{}",
                                    self.extras_folders_done,
                                    plural(self.extras_folders_total, "folder", "folders")
                                )),
                        );
                    }
                });
                ui.add_space(card_gap);

                // ── 3. Undo ──
                if !self.extras_last_job_loaded {
                    self.refresh_last_extras_job();
                }
                section_frame(ctx.style().as_ref()).show(ui, |ui| {
                    ui.spacing_mut().item_spacing = card_item_spacing;
                    fill_tile_width(ui);
                    ui.heading("↩ Undo");
                    match self.extras_last_job.clone() {
                        Some(job) => {
                            muted_label(
                                ui,
                                format!(
                                    "Last change: {} on {} — {} recorded. \
                                    Undo restores the original background image \
                                    contents and removes files the change created.",
                                    job.image_name,
                                    job.when,
                                    plural(job.folders, "set folder", "set folders")
                                ),
                            );
                            ui.horizontal_wrapped(|ui| {
                                if self.extras_rolling_back {
                                    if ui
                                        .button("⏹ Stop")
                                        .on_hover_text("Finishes the current folder, then stops; folders already restored are kept")
                                        .clicked()
                                    {
                                        self.stop_extras_rollback();
                                    }
                                    ui.add(egui::Spinner::new());
                                    muted_label(ui, self.rollback_progress.clone());
                                } else if ui
                                    .add_enabled(
                                        !self.extras_running,
                                        egui::Button::new("↩ Undo background change"),
                                    )
                                    .on_hover_text(
                                        "Asks for confirmation, then every replaced background file \
                                        gets its original content back, and files the change \
                                        created (for missing backgrounds) are removed.",
                                    )
                                    .clicked()
                                {
                                    // Overwrites every background file again,
                                    // so it confirms like the apply does.
                                    self.delete_confirmation =
                                        Some(DeleteIntent::ExtrasRollback {
                                            folders: job.folders,
                                            image_name: job.image_name.clone(),
                                        });
                                }
                            });
                            if self.extras_rolling_back {
                                let progress = if self.rollback_folders_total > 0 {
                                    self.rollback_folders_done as f32
                                        / self.rollback_folders_total as f32
                                } else {
                                    0.0
                                };
                                ui.add(egui::ProgressBar::new(progress).show_percentage().text(
                                    format!(
                                        "{}/{}",
                                        self.rollback_folders_done,
                                        plural(self.rollback_folders_total, "folder", "folders")
                                    ),
                                ));
                            }
                        }
                        None => {
                            muted_label(
                                ui,
                                "No background change to undo yet.",
                            );
                        }
                    }
                });
                ui.add_space(card_gap);

                // ── 4. Activity log ──
                if !self.extras_log.is_empty() {
                    section_frame(ctx.style().as_ref()).show(ui, |ui| {
                        ui.spacing_mut().item_spacing = card_item_spacing;
                        fill_tile_width(ui);
                        ui.heading("Activity");
                        muted_label(
                            ui,
                            format!(
                                "{} ok · {} failed · {} replaced{}{}",
                                plural(self.extras_successes, "folder", "folders"),
                                plural(self.extras_failures, "folder", "folders"),
                                plural(self.extras_files_replaced, "background file", "background files"),
                                if self.extras_files_cached > 0 {
                                    format!(" · {} already up to date", self.extras_files_cached)
                                } else {
                                    String::new()
                                },
                                if self.extras_files_skipped > 0 {
                                    format!(
                                        " · {} skipped (unsupported format)",
                                        self.extras_files_skipped
                                    )
                                } else {
                                    String::new()
                                },
                            ),
                        );
                        egui::ScrollArea::vertical()
                            .id_source("extras_log")
                            .max_height(160.0)
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                for entry in self.extras_log.iter().rev().take(40) {
                                    let dot = match entry.status {
                                        RepairLogStatus::Success => "✓",
                                        RepairLogStatus::Failed => "✗",
                                        RepairLogStatus::InProgress => "…",
                                        RepairLogStatus::Skipped => "–",
                                    };
                                    ui.label(format!("{dot} {}: {}", entry.label, entry.message));
                                }
                            });
                    });
                }
            });
    }

    fn render_collections_page(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let mut load_selected = false;
        let mut save_selection = false;
        let mut new_collection = false;
        let mut add_to_collection = false;
        let mut selected_collection_name: Option<String> = None;
        let mut rename_to: Option<String> = None;
        let mut add_all_matching: Option<String> = None;

        let picked_collection = self
            .selected_collection_index
            .and_then(|i| self.collections.get(i))
            .map(|c| c.name.clone());
        // Typing a different name while a collection is picked used to turn
        // "Save" into a hidden delete-and-recreate; that intent is now an
        // explicit Rename action and Save steps aside.
        let name_differs_from_picked = picked_collection.as_deref().is_some_and(|picked| {
            !self.collection_name.trim().is_empty() && picked != self.collection_name.trim()
        });

        let content_width = ui.available_width().max(1.0);
        egui::ScrollArea::vertical()
            .id_source("collections_page")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                fix_ui_width(ui, content_width);
                section_frame(ctx.style().as_ref()).show(ui, |ui| {
                    fill_tile_width(ui);
                    ui.horizontal(|ui| {
                        ui.heading("Collections");
                        ui.label(plural(self.selected_maps.len(), "selected map", "selected maps"));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("Reload from disk").clicked() {
                                self.load_collections();
                            }
                        });
                    });
                    muted_label(
                        ui,
                        "Pick maps in Library, then save them here. Close osu! before saving.",
                    );
                    ui.add_space(4.0);

                    // Inline result of the last action, at the point of the
                    // buttons that caused it — not only in the status bar.
                    if let Some(notice) = &self.collection_notice {
                        let (fill, stroke, text) = if notice.ok {
                            (
                                egui::Color32::from_rgb(0x1f, 0x2e, 0x24),
                                egui::Color32::from_rgb(0x2d, 0x8a, 0x4f),
                                egui::Color32::from_rgb(0x9d, 0xd0, 0xaa),
                            )
                        } else {
                            (
                                egui::Color32::from_rgb(0x33, 0x20, 0x23),
                                egui::Color32::from_rgb(0xc2, 0x6b, 0x72),
                                egui::Color32::from_rgb(0xe0, 0xa5, 0xab),
                            )
                        };
                        egui::Frame::none()
                            .fill(fill)
                            .stroke(egui::Stroke::new(1.0_f32, stroke))
                            .rounding(6.0)
                            .inner_margin(egui::Margin::symmetric(10.0, 6.0))
                            .show(ui, |ui| {
                                fill_tile_width(ui);
                                ui.label(egui::RichText::new(&notice.message).color(text));
                            });
                        ui.add_space(4.0);
                    }

                    // Single collection picker — the only one on this page,
                    // with this collection's own actions beside it.
                    ui.label(egui::RichText::new("Collection").strong());
                    ui.horizontal(|ui| {
                        let selected_name = picked_collection
                            .as_deref()
                            .unwrap_or("Choose collection");
                        egui::ComboBox::from_id_source("collections_picker")
                            .selected_text(selected_name)
                            .width((ui.available_width() - 250.0).max(160.0))
                            .show_ui(ui, |ui| {
                                for (i, collection) in self.collections.iter().enumerate() {
                                    let auto_adds = self
                                        .auto_collections
                                        .collections
                                        .get(&collection.name)
                                        .is_some_and(|config| config.enabled);
                                    let label = format!(
                                        "{}{} ({})",
                                        collection.name,
                                        if auto_adds { " ⚡" } else { "" },
                                        plural(collection.hashes.len(), "map", "maps")
                                    );
                                    ui.selectable_value(
                                        &mut self.selected_collection_index,
                                        Some(i),
                                        label,
                                    );
                                }
                            });
                        if ui
                            .add_enabled(
                                self.selected_collection_index.is_some(),
                                egui::Button::new("Open collection"),
                            )
                            .on_hover_text("Show the picked collection's maps and load them into the selection")
                            .clicked()
                        {
                            load_selected = true;
                        }
                        if ui
                            .add_enabled(
                                self.selected_collection_index.is_some(),
                                egui::Button::new("🗑"),
                            )
                            .on_hover_text("Delete the picked collection")
                            .clicked()
                            && let Some(name) = picked_collection.clone()
                        {
                            self.delete_confirmation = Some(DeleteIntent::Collection(name));
                        }
                    });

                    ui.add_space(4.0);
                    ui.separator();
                    // Save workflow: full-width name field, actions below it.
                    ui.label(egui::RichText::new("Save selection").strong());
                    // The app theme forces full-brightness text everywhere,
                    // which also makes hint text indistinguishable from real
                    // input — clear the override for this field so the
                    // placeholder renders faded as users expect.
                    ui.scope(|ui| {
                        ui.visuals_mut().override_text_color = None;
                        ui.add_sized(
                            [ui.available_width().max(80.0), 28.0],
                            egui::TextEdit::singleline(&mut self.collection_name)
                                .hint_text("Collection name, e.g. My favourites")
                                .vertical_align(egui::Align::Center),
                        );
                    });
                    ui.horizontal_wrapped(|ui| {
                        // The one filled button on the page: this is the
                        // app's whole purpose.
                        let save_hover = if name_differs_from_picked {
                            "The name differs from the picked collection — use Rename to rename it, or pick no collection to save as a new one".to_owned()
                        } else {
                            "Overwrite the named collection with the current selection".to_owned()
                        };
                        if ui
                            .add_enabled(
                                !self.collection_name.trim().is_empty()
                                    && !name_differs_from_picked,
                                egui::Button::new(
                                    egui::RichText::new("Save collection")
                                        .strong()
                                        .color(egui::Color32::from_rgb(0x1c, 0x1d, 0x21)),
                                )
                                .fill(egui::Color32::from_rgb(0xd8, 0x9a, 0xb0)),
                            )
                            .on_hover_text(save_hover)
                            .clicked()
                        {
                            save_selection = true;
                        }
                        if ui
                            .add_enabled(
                                !self.collection_name.trim().is_empty(),
                                egui::Button::new("Create empty"),
                            )
                            .on_hover_text("Create an empty collection with this name")
                            .clicked()
                        {
                            new_collection = true;
                        }
                        let add_label = match picked_collection.as_deref() {
                            Some(picked) => {
                                let shown: String = picked.chars().take(24).collect();
                                let shown =
                                    if picked.chars().count() > 24 { format!("{shown}…") } else { shown };
                                format!("Add maps to “{shown}”")
                            }
                            None => "Add maps to a collection".to_owned(),
                        };
                        if ui
                            .add_enabled(
                                self.selected_collection_index.is_some(),
                                egui::Button::new(add_label),
                            )
                            .on_hover_text("Append the current selection to the picked collection")
                            .clicked()
                        {
                            selected_collection_name = picked_collection.clone();
                            add_to_collection = true;
                        }
                        if name_differs_from_picked {
                            let picked = picked_collection.as_deref().unwrap_or_default();
                            let new_name = self.collection_name.trim();
                            let shown: String = new_name.chars().take(24).collect();
                            let shown = if new_name.chars().count() > 24 {
                                format!("{shown}…")
                            } else {
                                shown
                            };
                            if ui
                                .button(format!("Rename to “{shown}”"))
                                .on_hover_text(format!(
                                    "Rename “{picked}” to “{new_name}”, keeping its maps"
                                ))
                                .clicked()
                            {
                                rename_to = Some(new_name.to_owned());
                            }
                        }
                    });

                    ui.add_space(4.0);
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("Export list").on_hover_text("Write the selected maps to a plain-text spreadsheet list (TSV — opens in Excel or Google Sheets) in the app data folder").clicked() {
                            self.export_manifest();
                        }
                        let backup = self.cached_collection_backup_info();
                        let undo_hover = match &backup {
                            Some(info) => format!(
                                "Undo the last save: restore the backup from {} ({}, {})",
                                info.when,
                                plural(info.collections, "collection", "collections"),
                                plural(info.maps, "map", "maps")
                            ),
                            None => "No backup yet — one is created automatically on the first save".to_owned(),
                        };
                        if ui
                            .add_enabled(backup.is_some(), egui::Button::new("Undo last save"))
                            .on_hover_text(undo_hover)
                            .clicked()
                        {
                            self.delete_confirmation = Some(DeleteIntent::RestoreBackup);
                        }
                    });

                    ui.add_space(4.0);
                    ui.separator();
                    ui.label(egui::RichText::new("Auto-add new maps").strong());
                    match picked_collection.as_deref() {
                        Some(picked) => {
                            let mut enabled = self
                                .auto_collections
                                .collections
                                .get(picked)
                                .is_some_and(|config| config.enabled);
                            if ui
                                .checkbox(
                                    &mut enabled,
                                    "Add newly downloaded maps matching these filters",
                                )
                                .on_hover_text(
                                    "Runs whenever a scan discovers maps you don't have yet \
                                     (downloads, updates). Matching difficulties are added per \
                                     map — non-matching difficulties of the same set are \
                                     ignored. Maps already in your library are never touched.",
                                )
                                .changed()
                            {
                                let config = self
                                    .auto_collections
                                    .collections
                                    .entry(picked.to_owned())
                                    .or_default();
                                config.enabled = enabled;
                                self.save_auto_collections();
                            }
                            if enabled {
                                let mut auto_changed = false;
                                if let Some(config) =
                                    self.auto_collections.collections.get_mut(picked)
                                {
                                    let filters = &mut config.filters;
                                    muted_label(
                                        ui,
                                        "Filters apply per difficulty, like the Library filters.",
                                    );
                                    ui.add_space(2.0);
                                    let chips = filter_chips(filters);
                                    if chips.is_empty() {
                                        muted_label(
                                            ui,
                                            "No filters set — every new map will be added.",
                                        );
                                    } else {
                                        ui.horizontal_wrapped(|ui| {
                                            ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
                                            for chip in chips {
                                                egui::Frame::none()
                                                    .fill(egui::Color32::from_rgb(0x3a, 0x2b, 0x33))
                                                    .rounding(4.0)
                                                    .inner_margin(egui::Margin::symmetric(6.0, 2.0))
                                                    .show(ui, |ui| {
                                                        ui.label(egui::RichText::new(chip).small());
                                                    });
                                            }
                                        });
                                    }
                                    ui.add_space(4.0);
                                    egui::CollapsingHeader::new("⭐ Difficulty")
                                        .default_open(true)
                                        .show(ui, |ui| {
                                            auto_changed |= filter_range_row(
                                                ui,
                                                "Stars",
                                                &mut filters.stars,
                                                STARS_RANGE,
                                                0.1,
                                                1,
                                            );
                                            auto_changed |= filter_range_row(
                                                ui,
                                                "AR",
                                                &mut filters.ar,
                                                AR_RANGE,
                                                0.1,
                                                1,
                                            );
                                            auto_changed |= filter_range_row(
                                                ui,
                                                "CS",
                                                &mut filters.cs,
                                                CS_RANGE,
                                                0.1,
                                                1,
                                            );
                                            auto_changed |= filter_range_row(
                                                ui,
                                                "OD",
                                                &mut filters.od,
                                                OD_RANGE,
                                                0.1,
                                                1,
                                            );
                                            auto_changed |= filter_range_row(
                                                ui,
                                                "HP",
                                                &mut filters.hp,
                                                HP_RANGE,
                                                0.1,
                                                1,
                                            );
                                            auto_changed |= filter_range_row(
                                                ui,
                                                "BPM",
                                                &mut filters.bpm,
                                                BPM_RANGE,
                                                1.0,
                                                0,
                                            );
                                        });
                                    egui::CollapsingHeader::new("🔍 Search words")
                                        .default_open(false)
                                        .show(ui, |ui| {
                                            auto_changed |=
                                                filter_text_row(ui, "Artist", &mut filters.artist);
                                            auto_changed |=
                                                filter_text_row(ui, "Title", &mut filters.title);
                                            auto_changed |= filter_text_row(
                                                ui,
                                                "Mapper",
                                                &mut filters.mapper,
                                            );
                                            auto_changed |= filter_text_row(
                                                ui,
                                                "Difficulty",
                                                &mut filters.difficulty,
                                            );
                                            auto_changed |=
                                                filter_text_row(ui, "Tag", &mut filters.tag);
                                            muted_label(ui, "Empty means anything.");
                                        });
                                    egui::CollapsingHeader::new("🎵 Song")
                                        .default_open(false)
                                        .show(ui, |ui| {
                                            ui.label(
                                                egui::RichText::new("Length (seconds)").strong(),
                                            );
                                            ui.horizontal(|ui| {
                                                let box_width =
                                                    ((ui.available_width() - 28.0) / 2.0).max(60.0);
                                                auto_changed |= ui
                                                    .add_sized(
                                                        [box_width, 28.0],
                                                        egui::TextEdit::singleline(
                                                            &mut filters.length_min,
                                                        )
                                                        .hint_text("Min")
                                                        .vertical_align(egui::Align::Center),
                                                    )
                                                    .changed();
                                                ui.label("–");
                                                auto_changed |= ui
                                                    .add_sized(
                                                        [box_width, 28.0],
                                                        egui::TextEdit::singleline(
                                                            &mut filters.length_max,
                                                        )
                                                        .hint_text("Max")
                                                        .vertical_align(egui::Align::Center),
                                                    )
                                                    .changed();
                                            });
                                            if !filters.length_valid() {
                                                ui.label(
                                                    egui::RichText::new(
                                                        "Enter numbers, e.g. 60 and 180.",
                                                    )
                                                    .small()
                                                    .color(egui::Color32::from_rgb(
                                                        0xc4, 0xa2, 0x6a,
                                                    )),
                                                );
                                            }
                                            ui.add_space(4.0);
                                            ui.label(egui::RichText::new("Mode").strong());
                                            egui::ComboBox::from_id_source(
                                                "auto_add_mode_filter",
                                            )
                                            .selected_text(filters.mode.label())
                                            .width(ui.available_width().max(64.0))
                                            .show_ui(ui, |ui| {
                                                for mode in ModeFilter::ALL {
                                                    if ui
                                                        .selectable_value(
                                                            &mut filters.mode,
                                                            mode,
                                                            mode.label(),
                                                        )
                                                        .changed()
                                                    {
                                                        auto_changed = true;
                                                    }
                                                }
                                            });
                                        });
                                }
                                if auto_changed {
                                    self.save_auto_collections();
                                }
                                if self.scan.is_some() {
                                    ui.add_space(4.0);
                                    let (matching, total) = self.cached_auto_match_count();
                                    ui.horizontal(|ui| {
                                        ui.label(format!(
                                            "{matching} of {total} library maps match"
                                        ));
                                        if ui
                                            .button("Add all matching now")
                                            .on_hover_text(
                                                "Add every map in the library matching these \
                                                 filters to the collection right now",
                                            )
                                            .clicked()
                                        {
                                            add_all_matching = Some(picked.to_owned());
                                        }
                                    });
                                }
                            }
                        }
                        None => {
                            muted_label(
                                ui,
                                "Choose a collection above to give it auto-add filters.",
                            );
                        }
                    }

                    ui.separator();
                    ui.label(egui::RichText::new("Contents").strong());
                    if self.collections.is_empty() {
                        muted_label(ui, "No collections loaded. Pick maps in Library, name the collection above, and press Save collection.");
                    } else if self.selected_collection_index.is_none() {
                        muted_label(ui, "Choose a collection above to review its maps.");
                    } else if self.selected_collection_index.is_some() {
                        self.refresh_collection_contents();
                        // Selection toggles are collected while rendering and
                        // applied afterwards, so the row loop only needs shared
                        // borrows and the scan-wide lookup stays cached instead
                        // of being rebuilt from the whole library every frame.
                        let mut pending: Vec<(bool, String)> = Vec::new();
                        let matched = self.collection_contents_cache.len();
                        let selected_index =
                            self.selected_collection_index.expect("just checked");
                        let hash_count = self
                            .collections
                            .get(selected_index)
                            .map_or(0, |collection| collection.hashes.len());
                        muted_label(
                            ui,
                            format!(
                                "{}, {} in your scanned library",
                                plural(hash_count, "map", "maps"),
                                matched
                            ),
                        );
                        egui::ScrollArea::vertical()
                            .id_source("collections_page_maps")
                            .max_height(360.0)
                            .show(ui, |ui| {
                                let mut shown = 0_usize;
                                for hash_index in 0..hash_count {
                                    if shown >= 200 {
                                        break;
                                    }
                                    let (hash, label) = {
                                        let Some(hash) = self
                                            .collections
                                            .get(selected_index)
                                            .and_then(|collection| {
                                                collection.hashes.get(hash_index)
                                            })
                                        else {
                                            break;
                                        };
                                        let label = self
                                            .collection_contents_cache
                                            .get(hash)
                                            .map(|map| self.map_result_label(map));
                                        (hash.clone(), label)
                                    };
                                    let mut selected = self.selected_md5s.contains(&hash);
                                    let changed = if let Some(label) = label {
                                        ui.horizontal(|ui| {
                                            let changed =
                                                ui.checkbox(&mut selected, "").changed();
                                            ui.label(label);
                                            changed
                                        })
                                        .inner
                                    } else {
                                        ui.horizontal(|ui| {
                                            let changed =
                                                ui.checkbox(&mut selected, "").changed();
                                            ui.label(egui::RichText::new("Not installed").weak())
                                                .on_hover_text(format!("Map hash {hash}"));
                                            changed
                                        })
                                        .inner
                                    };
                                    if changed {
                                        pending.push((selected, hash));
                                    }
                                    shown += 1;
                                }
                                if hash_count > shown {
                                    muted_label(
                                        ui,
                                        format!("{} more", plural(hash_count - shown, "map", "maps")),
                                    );
                                }
                            });
                        for (select, hash) in pending {
                            if select {
                                if let Some(map) =
                                    self.collection_contents_cache.get(&hash).cloned()
                                {
                                    self.select_map(&map);
                                } else {
                                    self.select_missing_hash(&hash);
                                }
                            } else {
                                self.deselect_md5(&hash);
                            }
                        }
                    }

                    if !self.collection_missing_hashes.is_empty() {
                        muted_label(
                            ui,
                            format!(
                                "{} {} not installed locally — {} kept when you save.",
                                self.collection_missing_hashes.len(),
                                if self.collection_missing_hashes.len() == 1 {
                                    "map in this collection is"
                                } else {
                                    "maps in this collection are"
                                },
                                if self.collection_missing_hashes.len() == 1 {
                                    "it is"
                                } else {
                                    "they are"
                                },
                            ),
                        );
                    }
                });
            });

        if load_selected {
            self.load_selected_collection_into_selection();
        }
        if save_selection {
            let name = self.collection_name.trim().to_owned();
            let maps = self.selected_maps.len();
            let hashes = self.selected_md5s.len();
            let exists = self.collections.iter().any(|c| c.name == name);
            self.collection_name = name.clone();
            self.delete_confirmation = Some(DeleteIntent::SaveCollection {
                name,
                maps,
                hashes,
                exists,
            });
        }
        if new_collection {
            self.create_collection(&self.collection_name.clone());
        }
        if add_to_collection && let Some(name) = selected_collection_name.as_deref() {
            self.add_selected_to_collection(name);
        }
        if let Some(new_name) = rename_to
            && let Some(old_name) = picked_collection
        {
            self.delete_confirmation = Some(DeleteIntent::RenameCollection {
                old: old_name,
                new: new_name,
            });
        }
        if let Some(name) = add_all_matching {
            self.add_all_matching_to_collection(&name);
        }
    }

    fn maybe_show_delete_confirmation(&mut self, ctx: &egui::Context) {
        let Some(intent) = self.delete_confirmation.clone() else {
            return;
        };

        let (title, message, confirm_label) = match &intent {
            DeleteIntent::Collection(name) => (
                "Delete collection?",
                format!("Delete collection \"{name}\"? This cannot be undone."),
                "Delete",
            ),
            DeleteIntent::NonStdModes(count) => (
                "Delete non-std maps?",
                format!(
                    "Permanently delete {} from disk? \
                     This cannot be undone.",
                    plural(*count, "difficulty", "difficulties")
                ),
                "Delete files",
            ),
            DeleteIntent::RenameCollection { old, new } => (
                "Rename collection?",
                format!(
                    "Rename collection \"{old}\" to \"{new}\"?\n\nThe collection's maps are kept."
                ),
                "Rename",
            ),
            DeleteIntent::RestoreBackup => {
                let current_collections = self.collections.len();
                let current_maps: usize = self.collections.iter().map(|c| c.hashes.len()).sum();
                let message = match self.cached_collection_backup_info() {
                    Some(info) => format!(
                        "Undo the last save and return to the backup from {}?\n\nBackup: {}, {}.\nCurrent: {}, {}.\n\nAnything saved since the backup will be lost.",
                        info.when,
                        plural(info.collections, "collection", "collections"),
                        plural(info.maps, "map", "maps"),
                        plural(current_collections, "collection", "collections"),
                        plural(current_maps, "map", "maps")
                    ),
                    None => "No backup is available.".to_owned(),
                };
                ("Undo last save?", message, "Undo")
            }
            DeleteIntent::SaveCollection {
                name,
                maps,
                hashes,
                exists,
            } => {
                let message = if *exists {
                    format!(
                        "Overwrite collection \"{name}\" with the current selection?\n\n{} will be written ({} in total, including entries not installed locally).\nThe previous contents will be replaced (a backup is kept for undo).",
                        plural(*maps, "scanned map", "scanned maps"),
                        hashes
                    )
                } else {
                    format!(
                        "Create collection \"{name}\" with the current selection?\n\n{} will be written ({} in total).",
                        plural(*maps, "scanned map", "scanned maps"),
                        hashes
                    )
                };
                ("Save collection?", message, "Save")
            }
            DeleteIntent::SetBackground { sets } => (
                "Replace every background?",
                format!(
                    "Write the imported image over every background file referenced \
                     by charts in {}?\n\n\
                     Chart files (.osu/.osb) are never opened for writing, so map \
                     checksums, local scores and score submission are unaffected. \
                     The original images are backed up for undo.\n\n\
                     Close osu! first.",
                    plural(*sets, "scanned set folder", "scanned set folders")
                ),
                "Replace backgrounds",
            ),
            DeleteIntent::ShrinkRun { sets } => (
                "Shrink now?",
                format!(
                    "Compress song audio, background videos and images in {}?\n\n\
                     Each set is backed up first when “Back up each set” is on. \
                     Close osu! first.",
                    plural(*sets, "analyzed set", "analyzed sets")
                ),
                "Shrink",
            ),
            DeleteIntent::RestoringShrinkBackup { folder, .. } => (
                "Restore this backup?",
                format!(
                    "Replace the current contents of \"{folder}\" with the backup \
                     made before shrinking?\n\n\
                     Changes made after the backup will be lost."
                ),
                "Restore",
            ),
            DeleteIntent::ExtrasRollback {
                folders,
                image_name,
            } => (
                "Undo background change?",
                format!(
                    "Undo the last background apply of \"{image_name}\" ({})?\n\n\
                     Every replaced background file gets its original content back, \
                     and files the apply created are removed again.",
                    plural(*folders, "set folder", "set folders")
                ),
                "Undo",
            ),
        };

        let mut confirmed = false;
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(message);
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        self.delete_confirmation = None;
                    }
                    if ui.button(confirm_label).clicked() {
                        confirmed = true;
                    }
                });
            });

        if confirmed {
            self.delete_confirmation = None;
            match intent {
                DeleteIntent::Collection(_) => self.delete_selected_collection(),
                DeleteIntent::NonStdModes(_) => self.delete_selected_non_std_modes(),
                DeleteIntent::RestoreBackup => self.restore_collection_backup(),
                DeleteIntent::RenameCollection { old, new } => self.rename_collection(&old, &new),
                DeleteIntent::SaveCollection { name, .. } => {
                    self.collection_name = name;
                    self.save_selection_to_collection();
                }
                DeleteIntent::SetBackground { .. } => self.start_extras_apply(),
                DeleteIntent::ShrinkRun { .. } => self.start_shrink_run(),
                DeleteIntent::RestoringShrinkBackup { index, .. } => {
                    self.restore_shrink_backup(index)
                }
                DeleteIntent::ExtrasRollback { .. } => self.start_extras_rollback(),
            }
        }
    }

    fn maybe_show_app_update_window(&mut self, ctx: &egui::Context) {
        if !self.app_update_window_open {
            return;
        }
        let mut close = false;
        let mut check_now = false;
        let mut download_now = false;
        let mut open_releases = false;
        egui::Window::new(format!(
            "App update ({})",
            app_update::current_version_text()
        ))
        .collapsible(false)
        // The app's common dialog pattern: centered on open and fixed in
        // place — an anchored window cannot be dragged around or resized.
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .resizable(false)
        .default_width(420.0)
        .show(ctx, |ui| {
            ui.label(
                "Checks GitHub for a newer version. Downloading replaces the app and restarts it.",
            );
            ui.add_space(4.0);
            if !self.app_update_status.is_empty() {
                ui.label(self.app_update_status.clone());
            }
            if self.app_update_downloading {
                let progress = match self.app_update_total {
                    Some(total) if total > 0 => {
                        (self.app_update_downloaded as f32 / total as f32).clamp(0.0, 1.0)
                    }
                    _ => 0.0,
                };
                ui.add(egui::ProgressBar::new(progress).show_percentage());
            }
            if self.app_update_installing {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new());
                    ui.label("Installing… the app restarts automatically.");
                });
            }
            if let Some(release) = self.app_update_release.clone() {
                ui.add_space(4.0);
                ui.label(egui::RichText::new(format!("{} available", release.tag)).strong());
                if !release.notes.trim().is_empty() {
                    egui::ScrollArea::vertical()
                        .max_height(160.0)
                        .show(ui, |ui| {
                            ui.label(release.notes.clone());
                        });
                }
            } else if self.app_update_up_to_date {
                ui.add_space(4.0);
                ui.label("You're on the latest release.");
            }
            if let Some(error) = self.app_update_error.clone() {
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(error)
                        .small()
                        .color(egui::Color32::from_rgb(0xc4, 0x6a, 0x6a)),
                );
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Close").clicked() {
                    close = true;
                }
                let checking = self.app_update_checking
                    || self.app_update_downloading
                    || self.app_update_installing;
                if ui
                    .add_enabled(!checking, egui::Button::new("Check now"))
                    .clicked()
                {
                    check_now = true;
                }
                if let Some(release) = self.app_update_release.clone()
                    && ui
                        .add_enabled(!checking, egui::Button::new("Download & restart"))
                        .on_hover_text(format!("Downloads {}", release.asset_name))
                        .clicked()
                {
                    download_now = true;
                }
                if ui.small_button("Open releases page").clicked() {
                    open_releases = true;
                }
            });
        });
        // Repaint while a worker is active so progress stays live.
        if self.app_update_checking || self.app_update_downloading || self.app_update_installing {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        if close {
            self.app_update_window_open = false;
        }
        if check_now {
            self.start_app_update_check();
        }
        if download_now {
            self.start_app_update_download();
        }
        if open_releases {
            let _ = webbrowser::open(&format!(
                "https://github.com/{}/{}/releases/latest",
                app_update::GITHUB_OWNER,
                app_update::GITHUB_REPO
            ));
        }
    }

    fn cache_background(&mut self, path: PathBuf, texture: egui::TextureHandle) {
        if self.background_cache.contains_key(&path) {
            return;
        }
        while self.background_cache.len() >= BACKGROUND_CACHE_LIMIT {
            match self.background_cache_order.first().cloned() {
                Some(oldest) => {
                    self.background_cache_order.remove(0);
                    self.background_cache.remove(&oldest);
                }
                None => break,
            }
        }
        self.background_cache_order.push(path.clone());
        self.background_cache.insert(path, texture);
    }

    fn ensure_background_preview(&mut self, path: &Path, md5: &str) {
        if self.background_preview_path.as_deref() != Some(path) {
            self.background_preview_path = Some(path.to_owned());
            self.background_preview_error = None;

            if let Some(texture) = self.background_cache.get(path).cloned() {
                self.background_preview = Some(texture);
            } else {
                self.background_preview = None;
                self.spawn_background_decode(path);
            }
        }
        // Runs every frame while the inspector is visible, so a decode that
        // was skipped under load is retried as soon as a slot frees up.
        self.prefetch_neighbor_backgrounds(md5);
    }

    fn spawn_background_decode(&mut self, path: &Path) {
        if self.background_cache.contains_key(path)
            || self.background_in_flight.contains(path)
            || self.background_in_flight.len() >= BACKGROUND_MAX_IN_FLIGHT
        {
            return;
        }
        self.background_in_flight.insert(path.to_owned());
        let tx = self.background_load_tx.clone();
        let path = path.to_owned();
        thread::spawn(move || {
            let result = load_background_image(&path);
            let _ = tx.send((path, result));
        });
    }

    /// Decodes the backgrounds around the inspected map ahead of time so
    /// stepping to the next/previous beatmap usually hits the cache instead
    /// of paying a full decode on selection.
    fn prefetch_neighbor_backgrounds(&mut self, md5: &str) {
        let current = match &self.background_preview_path {
            Some(current) => current.clone(),
            None => return,
        };
        // Spend decode slots on the visible preview first.
        if !self.background_cache.contains_key(&current)
            && !self.background_in_flight.contains(&current)
        {
            return;
        }
        for path in self.neighbor_background_paths(md5) {
            if self.background_in_flight.len() >= BACKGROUND_MAX_IN_FLIGHT {
                break;
            }
            self.spawn_background_decode(&path);
        }
    }

    fn neighbor_background_paths(&mut self, md5: &str) -> Vec<PathBuf> {
        let position = self
            .map_index_for_md5(md5)
            .and_then(|map_index| self.filtered_pos_by_map_index.get(&map_index).copied());
        let Some(position) = position else {
            return Vec::new();
        };
        let Some(scan) = &self.scan else {
            return Vec::new();
        };
        let mut paths = Vec::new();
        // Nearest first so the closest maps win when decode slots run out.
        for distance in 1..=BACKGROUND_PREFETCH_RADIUS {
            for index in [
                position.checked_add(distance),
                position.checked_sub(distance),
            ]
            .into_iter()
            .flatten()
            {
                let Some(map_index) = self.filtered_map_indexes.get(index) else {
                    continue;
                };
                if let Some(map) = scan.maps.get(*map_index)
                    && let Some(filename) = &map.background_filename
                {
                    paths.push(map.folder.join(filename));
                }
            }
        }
        paths
    }

    fn clear_background_preview(&mut self) {
        self.background_preview_path = None;
        self.background_preview = None;
        self.background_preview_error = None;
    }

    /// Opens the shared output device on first use and reuses it afterwards.
    fn audio_handle(&mut self) -> Result<rodio::OutputStreamHandle> {
        if self.audio_backend.is_none() {
            let (stream, handle) = rodio::OutputStream::try_default()
                .context("opening the default audio output device")?;
            self.audio_backend = Some(AudioBackend {
                _stream: stream,
                handle,
            });
        }
        Ok(self
            .audio_backend
            .as_ref()
            .expect("backend was just created")
            .handle
            .clone())
    }

    fn start_audio_playback(&mut self, path: &Path) {
        self.audio_player = None;
        let handle = match self.audio_handle() {
            Ok(handle) => handle,
            Err(err) => {
                self.status = format!("Audio playback failed: {err:#}");
                return;
            }
        };
        match AudioPlayer::start(&handle, path, self.audio_volume) {
            Ok(player) => {
                self.status = format!("Playing audio in app: {}", path.display());
                self.audio_player = Some(player);
            }
            Err(err) => {
                // The device may have disappeared (unplugged/default changed):
                // drop the backend so the next attempt reopens it instead of
                // failing against a dead handle forever.
                self.audio_backend = None;
                self.status = format!("Audio playback failed: {err:#}");
            }
        }
    }

    fn toggle_audio_pause(&mut self) {
        let Some(player) = &self.audio_player else {
            return;
        };
        if player.sink.is_paused() {
            player.sink.play();
            self.status = format!("Resumed audio: {}", player.path.display());
        } else {
            player.sink.pause();
            self.status = format!("Paused audio: {}", player.path.display());
        }
    }

    fn stop_audio_playback(&mut self) {
        if let Some(player) = self.audio_player.take() {
            self.status = format!("Stopped audio: {}", player.path.display());
        }
    }

    fn poll_audio_playback(&mut self) {
        let finished = self
            .audio_player
            .as_ref()
            .is_some_and(|player| player.sink.empty());
        if finished && let Some(player) = self.audio_player.take() {
            self.status = format!("Finished audio: {}", player.path.display());
        }
    }

    fn open_map_path(&mut self, path: &Path, label: &str) {
        match open_with_default_app(path) {
            Ok(()) => self.status = format!("Opened {label}: {}", path.display()),
            Err(err) => self.status = format!("Could not open {label}: {err:#}"),
        }
    }
}

impl eframe::App for MapManagerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_background(ctx);
        self.skin_editor.poll(ctx);
        self.skin_editor.show_save_dialog(ctx);
        self.poll_audio_playback();
        if self.is_scanning
            || self.is_repairing
            || self.is_analyzing
            || self.is_shrinking
            || self.extras_running
            || self.extras_rolling_back
            || self.rollback_rx.is_some()
            || self.extras_image_rx.is_some()
            || self.audio_player.is_some()
            || self.app_update_checking
            || !self.background_in_flight.is_empty()
            || self.skin_editor.needs_repaint()
        {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }

        // Set from both checkbox sites (sidebar Advanced section and the
        // Maintenance card); persistence happens after the panels below.
        let mut ignore_backgrounds_toggled = false;
        // Sign-in from the top bar: switching to the Fix maps tab as well, so
        // the sign-in status and manual-URL fallback are visible.
        let mut topbar_sign_in_requested = false;

        egui::TopBottomPanel::top("top")
            .frame(panel_frame(ctx.style().as_ref()))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("osu! Map Manager");
                    ui.separator();
                    for tab in [
                        AppTab::Library,
                        AppTab::Collections,
                        AppTab::Maintenance,
                        AppTab::Shrink,
                        AppTab::SkinEditor,
                        AppTab::Extras,
                    ] {
                        let selected = self.active_tab == tab;
                        let text = if selected {
                            egui::RichText::new(tab.label()).strong()
                        } else {
                            egui::RichText::new(tab.label())
                        };
                        if ui
                            .selectable_label(selected, text)
                            .on_hover_text(tab.subtitle())
                            .clicked()
                        {
                            self.active_tab = tab;
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new(app_update::current_version_text())
                                .small()
                                .weak(),
                        )
                        .on_hover_text("Current app version");
                        // Gear, not "Update": the word collided with the map
                        // update actions on the Fix maps tab. The passive
                        // green highlight keeps an available release visible.
                        let (update_button, update_hover) = match &self.app_update_release {
                            Some(release) => (
                                egui::Button::new(
                                    egui::RichText::new(format!(
                                        "⚙ {} available",
                                        release.tag
                                    ))
                                    .strong()
                                    .color(egui::Color32::WHITE),
                                )
                                .fill(egui::Color32::from_rgb(0x2d, 0x8a, 0x4f)),
                                format!(
                                    "New app version {} available — click to open the updater",
                                    release.tag
                                ),
                            ),
                            None => (
                                egui::Button::new("⚙"),
                                "Check for app updates".to_owned(),
                            ),
                        };
                        let update_response = ui.add(update_button);
                        let update_clicked = update_response.clicked();
                        update_response.on_hover_text(update_hover);
                        if update_clicked {
                            self.app_update_window_open = true;
                        }
                        if let Some(player) = self.audio_player.as_ref() {
                            let paused = player.sink.is_paused();
                            let filename = player
                                .path
                                .file_name()
                                .and_then(|name| name.to_str())
                                .unwrap_or("map audio")
                                .to_owned();
                            if ui.small_button("■").on_hover_text("Stop audio").clicked() {
                                self.stop_audio_playback();
                            }
                            if ui
                                .small_button(if paused { "▶" } else { "⏸" })
                                .on_hover_text(if paused {
                                    "Resume audio"
                                } else {
                                    "Pause audio"
                                })
                                .clicked()
                            {
                                self.toggle_audio_pause();
                            }
                            ui.label(egui::RichText::new(format!("♪ {filename}")).small())
                                .on_hover_text(filename);
                        } else if self.oauth_session.is_some() {
                            ui.label(
                                egui::RichText::new("● signed in")
                                    .small()
                                    .color(egui::Color32::from_rgb(0x7f, 0xa6, 0x86)),
                            )
                            .on_hover_text(
                                "Signed in — downloads use the official osu! API",
                            );
                        } else if ui
                            .small_button("Sign in with osu!")
                            .on_hover_text(
                                "Sign in so downloads use the official osu! API instead of the mirror",
                            )
                            .clicked()
                        {
                            topbar_sign_in_requested = true;
                        }
                    });
                });
            });

        egui::SidePanel::left("sidebar")
            .resizable(false)
            .exact_width(320.0)
            .frame(panel_frame(ctx.style().as_ref()))
            .show(ctx, |ui| {
                // Solid (space-reserving) scrollbars here: the sidebar is full
                // of edge-to-edge fields and rails, which a floating bar would
                // paint over.
                ui.spacing_mut().scroll = egui::style::ScrollStyle::solid();
                egui::ScrollArea::vertical().show(ui, |ui| match self.active_tab {
                    AppTab::Library => {
                    ui.heading("Library");
                    muted_label(ui, "Point at your Songs folder and load your maps.");
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new("Songs folder").strong());
                    if self.songs_dir_editing {
                        ui.add_sized(
                            [ui.available_width().max(80.0), 28.0],
                            egui::TextEdit::singleline(&mut self.songs_dir)
                                .hint_text(r"C:\...\osu!\Songs")
                                .vertical_align(egui::Align::Center),
                        );
                        ui.horizontal(|ui| {
                            if ui.small_button("Done").clicked() {
                                self.songs_dir_editing = false;
                            }
                            muted_label(ui, "Folder containing your osu! Songs.");
                        });
                    } else if self.songs_dir.trim().is_empty() {
                        muted_label(ui, "Not set yet.");
                        if ui.button("Choose folder…").clicked() {
                            self.songs_dir_editing = true;
                        }
                    } else {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new("✓")
                                    .color(egui::Color32::from_rgb(0x7f, 0xa6, 0x86)),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui.small_button("Change…").clicked() {
                                        self.songs_dir_editing = true;
                                    }
                                    ui.add_sized(
                                        [ui.available_width().max(60.0), 20.0],
                                        egui::Label::new(
                                            egui::RichText::new(self.songs_dir.clone()).small(),
                                        )
                                        .truncate(true),
                                    );
                                },
                            );
                        });
                    }
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if self.is_scanning {
                            if ui.button("⏹ Stop scan").clicked() {
                                self.stop_scan();
                            }
                        } else if ui.button("⟳ Load my maps").clicked() {
                            self.start_scan();
                        }
                    });
                    if self.is_scanning {
                        muted_label(
                            ui,
                            scan_progress_status(
                                self.scanned_maps,
                                self.scan_total,
                                self.matched_maps,
                            ),
                        );
                    }

                    ui.add_space(8.0);
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.heading("Filters");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Clear").clicked() {
                                self.filters.clear_all();
                            }
                        });
                    });
                    // Active filters as readable chips, so a filter inside a
                    // collapsed group can never be invisible.
                    let chips = filter_chips(&self.filters);
                    if chips.is_empty() {
                        muted_label(ui, "A map must pass every active filter.");
                    } else {
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
                            for chip in chips {
                                egui::Frame::none()
                                    .fill(egui::Color32::from_rgb(0x3a, 0x2b, 0x33))
                                    .rounding(4.0)
                                    .inner_margin(egui::Margin::symmetric(6.0, 2.0))
                                    .show(ui, |ui| {
                                        ui.label(egui::RichText::new(chip).small());
                                    });
                            }
                        });
                    }
                    ui.add_space(6.0);

                    egui::CollapsingHeader::new("⭐ Difficulty")
                        .default_open(true)
                        .show(ui, |ui| {
                            muted_label(ui, "Enable, then drag the range. Drag a number for fine tuning (Shift = extra fine).");
                            filter_range_row(ui, "Stars", &mut self.filters.stars, STARS_RANGE, 0.1, 1);
                            filter_range_row(ui, "AR", &mut self.filters.ar, AR_RANGE, 0.1, 1);
                            filter_range_row(ui, "CS", &mut self.filters.cs, CS_RANGE, 0.1, 1);
                            filter_range_row(ui, "OD", &mut self.filters.od, OD_RANGE, 0.1, 1);
                            filter_range_row(ui, "HP", &mut self.filters.hp, HP_RANGE, 0.1, 1);
                            filter_range_row(ui, "BPM", &mut self.filters.bpm, BPM_RANGE, 1.0, 0);
                        });
                    egui::CollapsingHeader::new("🔍 Search words")
                        .default_open(false)
                        .show(ui, |ui| {
                            filter_text_row(ui, "Artist", &mut self.filters.artist);
                            filter_text_row(ui, "Title", &mut self.filters.title);
                            filter_text_row(ui, "Mapper", &mut self.filters.mapper);
                            filter_text_row(ui, "Difficulty", &mut self.filters.difficulty);
                            filter_text_row(ui, "Tag", &mut self.filters.tag);
                            muted_label(ui, "Empty means anything.");
                        });
                    egui::CollapsingHeader::new("🎵 Song")
                        .default_open(false)
                        .show(ui, |ui| {
                            ui.label(egui::RichText::new("Length (seconds)").strong());
                            ui.horizontal(|ui| {
                                let box_width = ((ui.available_width() - 28.0) / 2.0).max(60.0);
                                ui.add_sized(
                                    [box_width, 28.0],
                                    egui::TextEdit::singleline(&mut self.filters.length_min)
                                        .hint_text("Min")
                                        .vertical_align(egui::Align::Center),
                                );
                                ui.label("–");
                                ui.add_sized(
                                    [box_width, 28.0],
                                    egui::TextEdit::singleline(&mut self.filters.length_max)
                                        .hint_text("Max")
                                        .vertical_align(egui::Align::Center),
                                );
                            });
                            if !self.filters.length_valid() {
                                ui.label(
                                    egui::RichText::new("Enter numbers, e.g. 60 and 180.")
                                        .small()
                                        .color(egui::Color32::from_rgb(0xc4, 0xa2, 0x6a)),
                                );
                            }
                            ui.add_space(4.0);
                            ui.label(egui::RichText::new("Mode").strong());
                            egui::ComboBox::from_id_source("mode_filter")
                                .selected_text(self.filters.mode.label())
                                .width(ui.available_width().max(64.0))
                                .show_ui(ui, |ui| {
                                    for mode in ModeFilter::ALL {
                                        ui.selectable_value(&mut self.filters.mode, mode, mode.label());
                                    }
                                });
                        });
                    egui::CollapsingHeader::new("⚙ Advanced")
                        .default_open(false)
                        .show(ui, |ui| {
                            ui.checkbox(&mut self.skip_parse_timeouts, "Skip map parse timeouts");
                            ui.checkbox(&mut self.skip_parse_errors, "Skip map parse errors");
                            if ignore_backgrounds_checkbox(&mut self.repair_ignores, ui) {
                                ignore_backgrounds_toggled = true;
                            }
                            ui.add_space(4.0);
                            ui.label(egui::RichText::new("osu! search text").strong());
                            let mut query_text = self.filters.to_osu_search();
                            ui.add_sized(
                                [ui.available_width(), 56.0],
                                egui::TextEdit::multiline(&mut query_text)
                                    .interactive(false)
                                    .hint_text("No filters — matches everything"),
                            );
                        });
                    if self.filters.mode != self.last_mode_filter {
                        self.last_mode_filter = self.filters.mode;
                        let before = self.selected_maps.len();
                        if self.filters.mode != ModeFilter::Any {
                            let mode = self.filters.mode;
                            self.retain_selected_maps(|map| mode.matches(map.mode));
                        }
                        let removed = before - self.selected_maps.len();
                        if removed > 0 {
                            self.status = format!(
                                "Mode filter changed: {removed} selected {} no longer match and {} removed from the selection",
                                if removed == 1 { "map" } else { "maps" },
                                if removed == 1 { "was" } else { "were" },
                            );
                        }
                    }
                    }
                    AppTab::Collections => {
                        ui.heading("Collections");
                        muted_label(ui, "Your selected maps, ready to save.");
                        ui.separator();
                        ui.label(egui::RichText::new("Current selection").strong());
                        if self.selected_maps.is_empty() {
                            muted_label(ui, "Nothing picked yet — tick maps in the Library tab.");
                        } else {
                            ui.label(plural(self.selected_maps.len(), "map", "maps"));
                            // Peek at what is in the selection so "the
                            // selection" stays a concrete list, not a concept.
                            for map in self.selected_maps.iter().take(5) {
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(self.map_result_label(map)).small(),
                                    )
                                    .truncate(true),
                                );
                            }
                            if self.selected_maps.len() > 5 {
                                muted_label(
                                    ui,
                                    format!("…and {} more", self.selected_maps.len() - 5),
                                );
                            }
                        }
                        if !self.collection_missing_hashes.is_empty() {
                            ui.add_space(4.0);
                            muted_label(
                                ui,
                                format!(
                                    "{} {} not installed locally — kept while selected.",
                                    self.collection_missing_hashes.len(),
                                    if self.collection_missing_hashes.len() == 1 {
                                        "map is"
                                    } else {
                                        "maps are"
                                    },
                                ),
                            );
                        }
                        ui.add_space(4.0);
                        match self.cached_collection_backup_info() {
                            Some(info) => muted_label(ui, format!("Last backup: {}", info.when)),
                            None => muted_label(ui, "A backup is written on the first save."),
                        }
                        ui.add_space(4.0);
                        muted_label(ui, "Close osu! before saving.");
                    }
                    AppTab::Shrink => {
                        ui.heading("Shrink");
                        muted_label(ui, "Compress set assets to save disk.");
                        ui.separator();
                        if self.scan.is_some() {
                            ui.label(egui::RichText::new("Library").strong());
                            ui.label(format!(
                                "{} scanned",
                                plural(
                                    self.scan.as_ref().map(|s| s.sets.len()).unwrap_or(0),
                                    "set",
                                    "sets"
                                )
                            ));
                            ui.label(format!(
                                "{} analyzed · {} shrinkable",
                                plural(self.shrink_reports.len(), "set", "sets"),
                                plural(
                                    self.shrink_reports
                                        .iter()
                                        .map(|r| r.work_items())
                                        .sum::<usize>(),
                                    "file",
                                    "files"
                                )
                            ));
                        } else {
                            muted_label(ui, "Load your maps first (Library tab).");
                        }
                    }
                    AppTab::Maintenance => {
                        ui.heading("Fix maps");
                        muted_label(ui, "Keep your library healthy.");
                        ui.separator();
                        ui.label(egui::RichText::new("osu! account").strong());
                        scan_status_label(ui, "Status", &self.oauth_status.clone());
                        muted_label(
                            ui,
                            "Sign in (main panel) to repair via the official API; otherwise the mirror is used.",
                        );
                        ui.separator();
                        if let Some(scan) = &self.scan {
                            let ignore_missing_backgrounds =
                                self.repair_ignores.ignore_missing_backgrounds;
                            let missing = visible_problems(
                                &scan.problems,
                                ignore_missing_backgrounds,
                            )
                            .filter(|issue| {
                                issue.severity
                                    == crate::local::RepairSeverity::MissingRequiredFile
                            })
                            .count();
                            ui.label(egui::RichText::new("Library health").strong());
                            ui.label(format!(
                                "{} · {} · {}",
                                plural(scan.maps.len(), "map", "maps"),
                                plural(scan.sets.len(), "set", "sets"),
                                plural(
                                    visible_problems(&scan.problems, ignore_missing_backgrounds)
                                        .count(),
                                    "issue",
                                    "issues"
                                )
                            ));
                            ui.label(format!(
                                "{} missing-file · {} outdated",
                                missing,
                                self.outdated_sets.len()
                            ));
                        } else {
                            muted_label(ui, "Load your maps first to see health stats.");
                        }
                    }
                    AppTab::SkinEditor => {
                        let osu_root = self.osu_root();
                        let skins_dir = skins_dir_for(&osu_root);
                        let skin_cache = skin_cache_path_for(&osu_root);
                        self.skin_editor.sidebar(
                            ui,
                            skins_dir.as_deref(),
                            skin_cache.as_deref(),
                        );
                    }
                    AppTab::Extras => {
                        ui.heading("Backgrounds");
                        muted_label(ui, "Set one background for every map.");
                        ui.separator();
                        ui.label(egui::RichText::new("Imported background").strong());
                        match self.extras_image_name.as_deref() {
                            Some(name) => {
                                ui.label(name);
                                if let Some((width, height)) = self.extras_image_size {
                                    muted_label(ui, format!("{width}×{height} pixels"));
                                }
                                if self.extras_image_bytes.is_none() {
                                    muted_label(ui, "Still importing…");
                                }
                            }
                            None => muted_label(ui, "Nothing imported yet — pick one in the main panel."),
                        }
                        ui.separator();
                        ui.label(egui::RichText::new("Library").strong());
                        match self.scan.as_ref() {
                            Some(scan) => {
                                ui.label(plural(scan.sets.len(), "set", "sets"));
                                ui.label(plural(scan.maps.len(), "map", "maps"));
                            }
                            None => muted_label(ui, "Load your maps first (Library tab)."),
                        }
                    }
                });
            });

        self.refresh_filtered_maps();
        self.refresh_repair_jobs();

        let mut repair_requested = false;
        let mut repair_single_requested: Option<i64> = None;
        let mut update_check_requested = false;
        let mut update_check_pause_toggled = false;
        let mut update_check_stop_requested = false;
        let mut update_all_requested = false;
        let mut update_single_requested: Option<i64> = None;
        let mut update_pause_toggled = false;
        let mut update_stop_requested = false;
        let mut sign_in_requested = false;
        let mut sign_out_requested = false;
        let mut delete_non_std_requested = false;

        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(ctx.style().as_ref()))
            .show(ctx, |ui| {
                // Tabs now live in the top bar; the center shows one clear section header.
                ui.horizontal(|ui| {
                    ui.heading(self.active_tab.label());
                    muted_label(ui, self.active_tab.subtitle());
                });
                ui.add_space(6.0);

                match self.active_tab {
                    AppTab::Library => {
                        // The results card fills the central panel directly (no
                        // outer scroll), so the inner list/inspector scrollers
                        // stay aligned with their frames and heights stay bounded.
                        let card_item_spacing = ui.spacing().item_spacing;
                        let card_gap = 8.0;

                        if self.is_scanning {
                            section_frame(ctx.style().as_ref()).show(ui, |ui| {
                                ui.spacing_mut().item_spacing = card_item_spacing;
                                fill_tile_width(ui);
                                ui.horizontal(|ui| {
                                    ui.add(egui::Spinner::new());
                                    muted_label(ui, "Scanning library…");
                                });
                                if let Some(err) = &self.star_parse_error {
                                    ui.label(
                                        egui::RichText::new(
                                            "Couldn't read star ratings for some maps.",
                                        )
                                        .weak(),
                                    )
                                    .on_hover_text(err);
                                }
                            });
                            ui.add_space(card_gap);
                        }

                        if self.scan.is_none() {
                            section_frame(ctx.style().as_ref()).show(ui, |ui| {
                                ui.spacing_mut().item_spacing = card_item_spacing;
                                fill_tile_width(ui);
                                ui.heading("No scan yet");
                                muted_label(
                                    ui,
                                    "Enter your Songs folder in the sidebar, then press Load my maps.",
                                );
                            });
                        } else {
                            let (scanned_maps, scanned_sets, repair_issues) =
                                self.scan.as_ref().map_or((0, 0, 0), |scan| {
                                    (
                                        scan.maps.len(),
                                        scan.sets.len(),
                                        visible_problems(
                                            &scan.problems,
                                            self.repair_ignores.ignore_missing_backgrounds,
                                        )
                                        .count(),
                                    )
                            });
                            section_frame(ctx.style().as_ref()).show(ui, |ui| {
                                ui.spacing_mut().item_spacing = card_item_spacing;
                                fill_tile_width(ui);
                                ui.horizontal(|ui| {
                                    ui.heading("Results");
                                    ui.label(format!(
                                        "{} matching · {} selected",
                                        self.filtered_map_indexes.len(),
                                        self.selected_maps.len()
                                    ));
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if ui.button("Clear selection").clicked() {
                                                self.clear_selection();
                                            }
                                            let matching = self.filtered_map_indexes.len();
                                            if ui
                                                .add_enabled(
                                                    matching > 0,
                                                    egui::Button::new(format!(
                                                        "Select all {}",
                                                        plural(matching, "map", "maps")
                                                    )),
                                                )
                                                .clicked()
                                            {
                                                let maps = self
                                                    .scan
                                                    .as_ref()
                                                    .map(|scan| {
                                                        self.filtered_map_indexes
                                                            .iter()
                                                            .filter_map(|&index| {
                                                                scan.maps.get(index).cloned()
                                                            })
                                                            .collect::<Vec<_>>()
                                                    })
                                                    .unwrap_or_default();
                                                for map in &maps {
                                                    self.select_map(map);
                                                }
                                            }
                                            // The visible next step after picking
                                            // maps — switches to the Collections tab.
                                            if ui
                                                .add(
                                                    egui::Button::new(
                                                        egui::RichText::new("Save as collection →")
                                                            .strong(),
                                                    )
                                                    .fill(egui::Color32::from_rgb(
                                                        0x3a, 0x2b, 0x33,
                                                    )),
                                                )
                                                .clicked()
                                            {
                                                self.active_tab = AppTab::Collections;
                                            }
                                        },
                                    );
                                });
                                muted_label(
                                    ui,
                                    format!(
                                        "{} · {} · {}",
                                        plural(scanned_maps, "map", "maps"),
                                        plural(scanned_sets, "set", "sets"),
                                        plural(repair_issues, "issue", "issues"),
                                    ),
                                );
                                // Bounded: the central panel is not inside a scroll
                                // area, so this is the real remaining viewport height.
                                let list_height = ui.available_height().max(120.0);
                                self.render_map_workspace(ui, ctx, list_height);
                            });
                        }
                    }
                    AppTab::Collections => {
                        self.render_collections_page(ui, ctx);
                    }
                    AppTab::Shrink => {
                        self.render_shrink_page(ui, ctx);
                    }
                    AppTab::SkinEditor => {
                        self.skin_editor.center_page(ui, ctx);
                    }
                    AppTab::Extras => {
                        self.render_extras_page(ui, ctx);
                    }
                    AppTab::Maintenance => {
                        let content_width = ui.available_width().max(1.0);
                        egui::ScrollArea::vertical()
                            .id_source("repairs_delete_pane")
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                let card_item_spacing = ui.spacing().item_spacing;
                                let card_gap = 8.0;
                                ui.spacing_mut().item_spacing.y = 0.0;
                                fix_ui_width(ui, content_width);

                                if let Some(scan) = &self.scan {
                                    let jobs = &self.repair_jobs_cache;
                                    let ignore_missing_backgrounds =
                                        self.repair_ignores.ignore_missing_backgrounds;
                                    let missing_file_issues = visible_problems(
                                        &scan.problems,
                                        ignore_missing_backgrounds,
                                    )
                                    .filter(|issue| {
                                        issue.severity == RepairSeverity::MissingRequiredFile
                                    })
                                    .count();
                                    section_frame(ctx.style().as_ref()).show(ui, |ui| {
                                        ui.spacing_mut().item_spacing = card_item_spacing;
                                        fill_tile_width(ui);
                                        ui.heading("🔧 Repair missing files");
                                        muted_label(
                                            ui,
                                            format!(
                                                "{}, {} ready to repair",
                                                plural(missing_file_issues, "issue", "issues"),
                                                plural(jobs.len(), "set", "sets")
                                            ),
                                        );
                                        muted_label(
                                            ui,
                                            "Redownloads the set and restores only the missing files — scores and edits are kept.",
                                        );
                                        if ignore_backgrounds_checkbox(
                                            &mut self.repair_ignores,
                                            ui,
                                        ) {
                                            ignore_backgrounds_toggled = true;
                                        }
                                        muted_label(
                                            ui,
                                            "Turn on if backgrounds were deleted to save space: they disappear from issue counts and repairs. Missing audio is still reported.",
                                        );
                                        ui.horizontal_wrapped(|ui| {
                                            if self.oauth_session.is_some() {
                                                if ui.button("Sign out of osu!").clicked() {
                                                    sign_out_requested = true;
                                                }
                                            } else if ui
                                                .add_enabled(
                                                    !self.is_signing_in,
                                                    egui::Button::new("Sign in with osu!"),
                                                )
                                                .clicked()
                                            {
                                                sign_in_requested = true;
                                            }
                                            if self.is_signing_in {
                                                ui.add(egui::Spinner::new());
                                            }
                                        });
                                        scan_status_label(ui, "osu! sign-in", &self.oauth_status.clone());
                                        if self.is_signing_in {
                                            if let Some(mut pending_url) =
                                                self.oauth_pending_url.clone()
                                            {
                                                muted_label(
                                                    ui,
                                                    "If no browser tab opened, paste this URL into your browser manually:",
                                                );
                                                    ui.horizontal(|ui| {
                                                        let input_width =
                                                            (ui.available_width() - 60.0).max(120.0);
                                                        ui.add_sized(
                                                            [input_width, 24.0],
                                                            egui::TextEdit::singleline(&mut pending_url)
                                                                .interactive(false),
                                                        );
                                                        if ui.button("Copy").clicked() {
                                                            ui.ctx().copy_text(pending_url);
                                                        }
                                                    });
                                                    // A 401 here means the OAuth app's own
                                                    // callback registration is wrong — a
                                                    // setup problem, not something the
                                                    // player can fix from this screen.
                                                }
                                        } else if self.oauth_session.is_none() {
                                            muted_label(
                                                ui,
                                                "Without sign-in, downloads fall back to the mirror. Sign in to download via the official osu! API.",
                                            );
                                        }
                                        ui.horizontal(|ui| {
                                            if ui
                                                .add_enabled(
                                                    !self.is_repairing && !jobs.is_empty(),
                                                    egui::Button::new("Repair all"),
                                                )
                                                .clicked()
                                            {
                                                repair_requested = true;
                                            }
                                            if self.is_repairing {
                                                ui.add(egui::Spinner::new());
                                            }
                                        });
                                        if jobs.is_empty() && missing_file_issues > 0 {
                                            wrapped_label(ui, "These findings do not have a usable beatmapset ID. Rescan after this build; the app now infers IDs from osu! song folder names.");
                                        }
                                        if self.is_repairing {
                                            wrapped_label(ui, &self.repair_progress);
                                        }
                                        if self.repair_total > 0 {
                                            let progress =
                                                self.repair_done as f32 / self.repair_total as f32;
                                            ui.add(egui::ProgressBar::new(progress).text(format!(
                                                "{}/{} complete | {} succeeded | {} failed",
                                                self.repair_done,
                                                self.repair_total,
                                                self.repair_successes,
                                                self.repair_failures
                                            )));
                                        }
                                        if !jobs.is_empty() || !self.repair_log.is_empty() {
                                            ui.add_space(6.0);
                                            egui::ScrollArea::vertical()
                                                .id_source("repairable_sets")
                                                .max_height(280.0)
                                                .show(ui, |ui| {
                                                    let log_skip = self
                                                        .repair_log
                                                        .len()
                                                        .saturating_sub(MAX_RENDERED_REPAIR_LOG);
                                                    if log_skip > 0 {
                                                        muted_label(
                                                            ui,
                                                            format!(
                                                                "… {} earlier log lines hidden",
                                                                log_skip
                                                            ),
                                                        );
                                                    }
                                                    for entry in
                                                        &self.repair_log[log_skip..]
                                                    {
                                                        status_log_label(
                                                            ui,
                                                            entry.status,
                                                            "repairing",
                                                            "repaired",
                                                            "failed",
                                                            "skipped",
                                                            entry.beatmapset_id,
                                                            &entry.message,
                                                        );
                                                    }
                                                    for job in jobs.iter().take(MAX_RENDERED_REPAIR_JOBS)
                                                    {
                                                        let beatmapset_id = job.beatmapset_id;
                                                        nested_frame(ui.style()).show(ui, |ui| {
                                                            fill_tile_width(ui);
                                                            ui.horizontal(|ui| {
                                                                // "Artist - Title" reads far
                                                                // better than a bare set id.
                                                                wrapped_label(
                                                                    ui,
                                                                    job.title.clone().unwrap_or_else(
                                                                        || format!("Set {beatmapset_id}"),
                                                                    ),
                                                                );
                                                                if ui
                                                                    .add_enabled(
                                                                        !self.is_repairing,
                                                                        egui::Button::new(
                                                                            "Repair this set",
                                                                        ),
                                                                    )
                                                                    .clicked()
                                                                {
                                                                    repair_single_requested =
                                                                        Some(beatmapset_id);
                                                                }
                                                            });
                                                            muted_label(
                                                                ui,
                                                                format!(
                                                                    "set {} · {} with missing files",
                                                                    beatmapset_id,
                                                                    plural(
                                                                        job.labels.len(),
                                                                        "map",
                                                                        "maps"
                                                                    )
                                                                ),
                                                            );
                                                            if !job.missing_files.is_empty() {
                                                                wrapped_label(
                                                                    ui,
                                                                    format!(
                                                                        "  Missing: {}",
                                                                        job.missing_files.join(", ")
                                                                    ),
                                                                );
                                                            }
                                                            for issue in &job.issues {
                                                                wrapped_label(ui, format!("  {issue}"));
                                                            }
                                                        });
                                                    }
                                                    if jobs.len() > MAX_RENDERED_REPAIR_JOBS {
                                                        muted_label(
                                                            ui,
                                                            format!(
                                                                "… {} more sets hidden — “Repair all” still covers them",
                                                                jobs.len() - MAX_RENDERED_REPAIR_JOBS
                                                            ),
                                                        );
                                                    }
                                                });
                                        } else if visible_problems(
                                            &scan.problems,
                                            ignore_missing_backgrounds,
                                        )
                                        .count()
                                            > 0
                                        {
                                            ui.add_space(6.0);
                                            let visible_issues: Vec<_> = visible_problems(
                                                &scan.problems,
                                                ignore_missing_backgrounds,
                                            )
                                            .collect();
                                            egui::ScrollArea::vertical()
                                                .id_source("repair")
                                                .max_height(240.0)
                                                .show(ui, |ui| {
                                                    for issue in visible_issues
                                                        .iter()
                                                        .take(MAX_RENDERED_REPAIR_ISSUES)
                                                    {
                                                        let severity = match issue.severity {
                                                            RepairSeverity::MissingRequiredFile => {
                                                                "missing"
                                                            }
                                                            RepairSeverity::ParseWarning => "parse",
                                                        };
                                                        wrapped_label(
                                                            ui,
                                                            format!(
                                                                "{severity}: {} ({})",
                                                                issue.message,
                                                                issue.beatmap.display()
                                                            ),
                                                        );
                                                    }
                                                    if visible_issues.len()
                                                        > MAX_RENDERED_REPAIR_ISSUES
                                                    {
                                                        muted_label(
                                                            ui,
                                                            format!(
                                                                "… {} more {} hidden",
                                                                visible_issues.len()
                                                                    - MAX_RENDERED_REPAIR_ISSUES,
                                                                if visible_issues.len() - MAX_RENDERED_REPAIR_ISSUES == 1 { "issue" } else { "issues" }
                                                            ),
                                                        );
                                                    }
                                                });
                                        }
                                    });
                                    ui.add_space(card_gap);

                                    section_frame(ctx.style().as_ref()).show(ui, |ui| {
                                        ui.spacing_mut().item_spacing = card_item_spacing;
                                        fill_tile_width(ui);
                                        ui.heading("⬆ Update outdated maps");
                                        muted_label(
                                            ui,
                                            "Compares installed maps against osu!web and refreshes outdated sets in place. Checking works without sign-in; downloads prefer the official API when signed in.",
                                        );
                                        ui.horizontal(|ui| {
                                            if ui
                                                .add_enabled(
                                                    !self.is_checking_updates
                                                        && !self.is_updating
                                                        && !self.is_scanning,
                                                    egui::Button::new("Check for updates"),
                                                )
                                                .clicked()
                                            {
                                                update_check_requested = true;
                                            }
                                            if self.is_checking_updates || self.is_updating {
                                                if self.is_checking_updates {
                                                    let paused = self.update_check_paused();
                                                    if ui
                                                        .button(if paused {
                                                            "▶ Resume"
                                                        } else {
                                                            "⏸ Pause"
                                                        })
                                                        .on_hover_text(
                                                            "Pause takes effect after the current set; the in-flight request always finishes first",
                                                        )
                                                        .clicked()
                                                    {
                                                        update_check_pause_toggled = true;
                                                    }
                                                    if ui
                                                        .button("⏹ Stop")
                                                        .on_hover_text(
                                                            "Stops the check after the current request; sets already checked are kept",
                                                        )
                                                        .clicked()
                                                    {
                                                        update_check_stop_requested = true;
                                                    }
                                                }
                                                let paused = (self.is_checking_updates
                                                    && self.update_check_paused())
                                                    || (self.is_updating
                                                        && self.update_paused());
                                                if !paused {
                                                    ui.add(egui::Spinner::new());
                                                }
                                            }
                                        });
                                        if self.is_checking_updates {
                                            wrapped_label(
                                                ui,
                                                if self.update_check_paused() {
                                                    "Paused — resume to continue"
                                                } else {
                                                    &self.update_check_status
                                                },
                                            );
                                            if self.update_check_total > 0 {
                                                ui.add(
                                                    egui::ProgressBar::new(
                                                        self.update_check_done as f32
                                                            / self.update_check_total as f32,
                                                    )
                                                    .text(format!(
                                                        "{}/{} checked",
                                                        self.update_check_done,
                                                        self.update_check_total
                                                    )),
                                                );
                                            }
                                        } else if !self.update_check_status.is_empty() {
                                            wrapped_label(ui, &self.update_check_status);
                                            if self.update_unavailable > 0 {
                                                muted_label(
                                                    ui,
                                                    format!(
                                                        "{} {} no longer available online and were skipped",
                                                        self.update_unavailable,
                                                        if self.update_unavailable == 1 { "set is" } else { "sets are" }
                                                    ),
                                                );
                                            }
                                        }
                                        if let Some(note) = &self.update_check_db_note {
                                            // Own persistent row: progress messages must not be
                                            // able to flash it away, and it stays after finish.
                                            ui.label(
                                                egui::RichText::new(format!("⚠ {note}"))
                                                    .color(egui::Color32::from_rgb(
                                                        0xd1, 0x9a, 0x4a,
                                                    ))
                                                    .weak(),
                                            );
                                        }
                                        if !self.outdated_sets.is_empty() {
                                            ui.horizontal(|ui| {
                                                if ui
                                                    .add_enabled(
                                                        !self.is_updating
                                                            && !self.is_checking_updates,
                                                        egui::Button::new(format!(
                                                            "Update all ({})",
                                                            self.outdated_sets.len()
                                                        )),
                                                    )
                                                    .clicked()
                                                {
                                                    update_all_requested = true;
                                                }
                                                if self.is_updating {
                                                    let paused = self.update_paused();
                                                    if ui
                                                        .button(if paused {
                                                            "▶ Resume"
                                                        } else {
                                                            "⏸ Pause"
                                                        })
                                                        .on_hover_text(
                                                            "Pause takes effect after the current set; in-flight downloads always finish first",
                                                        )
                                                        .clicked()
                                                    {
                                                        update_pause_toggled = true;
                                                    }
                                                    if ui
                                                        .button("⏹ Stop after current set")
                                                        .on_hover_text(
                                                            "Finishes the in-flight download, then stops; sets already updated are kept",
                                                        )
                                                        .clicked()
                                                    {
                                                        update_stop_requested = true;
                                                    }
                                                    if !paused {
                                                        ui.add(egui::Spinner::new());
                                                    }
                                                }
                                            });
                                            if self.is_updating {
                                                wrapped_label(
                                                    ui,
                                                    if self.update_paused() {
                                                        "Paused — resume to continue"
                                                    } else {
                                                        &self.update_progress
                                                    },
                                                );
                                            }
                                            if self.update_total > 0 {
                                                let progress = self.update_done as f32
                                                    / self.update_total as f32;
                                                ui.add(
                                                    egui::ProgressBar::new(progress).text(
                                                        format!(
                                                            "{}/{} complete | {} succeeded | {} failed",
                                                            self.update_done,
                                                            self.update_total,
                                                            self.update_successes,
                                                            self.update_failures
                                                        ),
                                                    ),
                                                );
                                            }
                                            ui.add_space(6.0);
                                            egui::ScrollArea::vertical()
                                                .id_source("outdated_sets")
                                                .max_height(280.0)
                                                .show(ui, |ui| {
                                                    let log_skip = self
                                                        .update_log
                                                        .len()
                                                        .saturating_sub(MAX_RENDERED_REPAIR_LOG);
                                                    if log_skip > 0 {
                                                        muted_label(
                                                            ui,
                                                            format!(
                                                                "… {} earlier log lines hidden",
                                                                log_skip
                                                            ),
                                                        );
                                                    }
                                                    for entry in
                                                        &self.update_log[log_skip..]
                                                    {
                                                        status_log_label(
                                                            ui,
                                                            entry.status,
                                                            "updating",
                                                            "updated",
                                                            "failed",
                                                            "no longer online",
                                                            entry.beatmapset_id,
                                                            &entry.message,
                                                        );
                                                    }
                                                    for set in &self.outdated_sets {
                                                        let beatmapset_id = set.beatmapset_id;
                                                        nested_frame(ui.style()).show(
                                                            ui,
                                                            |ui| {
                                                                fill_tile_width(ui);
                                                                ui.horizontal(|ui| {
                                                                    let mut header = format!(
                                                                        "{} (set {}): {} of {} diffs outdated",
                                                                        set.title,
                                                                        beatmapset_id,
                                                                        set.outdated_count(),
                                                                        set.total_checked()
                                                                    );
                                                                    if !set.status.is_empty() {
                                                                        header.push_str(&format!(
                                                                            " · {}",
                                                                            set.status
                                                                        ));
                                                                    }
                                                                    if let Some(updated) =
                                                                        &set.remote_updated
                                                                    {
                                                                        header.push_str(&format!(
                                                                            " · online version {updated}"
                                                                        ));
                                                                    }
                                                                    wrapped_label(ui, header);
                                                                    if ui
                                                                        .add_enabled(
                                                                            !self.is_updating
                                                                                && !self.is_checking_updates,
                                                                            egui::Button::new(
                                                                                "Update",
                                                                            ),
                                                                        )
                                                                        .clicked()
                                                                    {
                                                                        update_single_requested =
                                                                            Some(beatmapset_id);
                                                                    }
                                                                });
                                                                for diff in &set.outdated {
                                                                    wrapped_label(
                                                                        ui,
                                                                        format!("  {}", diff.label),
                                                                    );
                                                                }
                                                                if set.unchecked > 0 {
                                                                    muted_label(
                                                                        ui,
                                                                        format!(
                                                                            "  {} without an online id could not be checked",
                                                                            plural(set.unchecked, "difficulty", "difficulties")
                                                                        ),
                                                                    );
                                                                }
                                                            },
                                                        );
                                                    }
                                                });
                                        } else if !self.is_checking_updates
                                            && self.update_check_total > 0
                                        {
                                            muted_label(
                                                ui,
                                                "All checked beatmaps are up to date.",
                                            );
                                        }
                                    });
                                    ui.add_space(card_gap);

                                    let delete_selection = DeleteModeSelection {
                                        taiko: self.delete_taiko,
                                        catch: self.delete_catch,
                                        mania: self.delete_mania,
                                    };
                                    let delete_count = scan
                                        .maps
                                        .iter()
                                        .filter(|map| delete_selection.matches(map.mode))
                                        .count();
                                    let mode_counts = count_modes(&scan.maps);
                                    ui.add_space(card_gap);
                                    section_frame(ctx.style().as_ref()).show(ui, |ui| {
                                        ui.spacing_mut().item_spacing = card_item_spacing;
                                        fill_tile_width(ui);
                                        ui.heading("🗑 Clean up non-std modes");
                                        muted_label(
                                            ui,
                                            format!(
                                                "Library: {} · {} taiko · {} catch · {} mania{}",
                                                plural(mode_counts.std, "std map", "std maps"),
                                                mode_counts.taiko,
                                                mode_counts.catch,
                                                mode_counts.mania,
                                                if mode_counts.unknown > 0 {
                                                    format!(
                                                        " · {} unrecognized",
                                                        mode_counts.unknown
                                                    )
                                                } else {
                                                    String::new()
                                                },
                                            ),
                                        );
                                        if mode_counts.taiko
                                            + mode_counts.catch
                                            + mode_counts.mania
                                            == 0
                                        {
                                            muted_label(
                                                ui,
                                                "No non-std difficulties were detected. Converted maps share the original std file, so only natively mapped taiko/catch/mania files can appear here — rescan if you added some.",
                                            );
                                        }
                                        ui.horizontal_wrapped(|ui| {
                                            ui.checkbox(&mut self.delete_taiko, "Taiko");
                                            ui.checkbox(&mut self.delete_catch, "Catch");
                                            ui.checkbox(&mut self.delete_mania, "Mania");
                                        });
                                        // Say what will happen per mode, in outcome terms.
                                        let mut selected_modes: Vec<String> = Vec::new();
                                        if self.delete_taiko && mode_counts.taiko > 0 {
                                            selected_modes.push(format!(
                                                "{} taiko",
                                                mode_counts.taiko
                                            ));
                                        }
                                        if self.delete_catch && mode_counts.catch > 0 {
                                            selected_modes.push(format!(
                                                "{} catch",
                                                mode_counts.catch
                                            ));
                                        }
                                        if self.delete_mania && mode_counts.mania > 0 {
                                            selected_modes.push(format!(
                                                "{} mania",
                                                mode_counts.mania
                                            ));
                                        }
                                        muted_label(
                                            ui,
                                            if selected_modes.is_empty() {
                                                "Tick the modes to delete.".to_owned()
                                            } else {
                                                format!(
                                                    "{} will be deleted permanently",
                                                    selected_modes.join(", ")
                                                )
                                            },
                                        );
                                        if ui
                                            .add_enabled(
                                                !self.is_scanning
                                                    && !self.is_repairing
                                                    && delete_count > 0,
                                                egui::Button::new("Delete selected non-std maps"),
                                            )
                                            .on_hover_text("A confirmation dialog states the count before anything is deleted")
                                            .clicked()
                                        {
                                            delete_non_std_requested = true;
                                        }
                                    });
                                } else {
                                    section_frame(ctx.style().as_ref()).show(ui, |ui| {
                                        ui.spacing_mut().item_spacing = card_item_spacing;
                                        fill_tile_width(ui);
                                        ui.heading("Maintenance");
                                        muted_label(ui, "Load your maps first — repairs, updates and cleanup appear here.");
                                    });
                                }
                            });
                    }
                }
            });

        // Bottom status bar — single place for progress + last message.
        egui::TopBottomPanel::bottom("status")
            .frame(panel_frame(ctx.style().as_ref()))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let status = if self.is_scanning {
                        scan_progress_status(self.scanned_maps, self.scan_total, self.matched_maps)
                    } else if self.is_repairing && !self.repair_progress.is_empty() {
                        self.repair_progress.clone()
                    } else if self.is_checking_updates && !self.update_check_status.is_empty() {
                        self.update_check_status.clone()
                    } else if self.is_updating && !self.update_progress.is_empty() {
                        self.update_progress.clone()
                    } else {
                        self.status.clone()
                    };
                    ui.add_sized(
                        [ui.available_width(), 18.0],
                        egui::Label::new(status.clone()).truncate(true),
                    )
                    .on_hover_text(status);
                });
            });

        if repair_requested {
            self.start_repair_all();
        }
        if let Some(beatmapset_id) = repair_single_requested {
            self.start_repair_single(beatmapset_id);
        }
        if ignore_backgrounds_toggled {
            // The repair-jobs cache key includes the flag, so counts and jobs
            // update on the next refresh; only the persistence is left here.
            match save_repair_ignores(&self.osu_root(), &self.repair_ignores) {
                Ok(()) => {
                    self.status = if self.repair_ignores.ignore_missing_backgrounds {
                        "Missing backgrounds are now ignored".to_owned()
                    } else {
                        "Missing backgrounds are checked again".to_owned()
                    };
                }
                Err(err) => {
                    self.status = format!("Saving repair settings failed: {err:#}");
                }
            }
        }
        if update_check_requested {
            self.start_update_check();
        }
        if update_check_pause_toggled {
            self.toggle_update_check_pause();
        }
        if update_check_stop_requested {
            self.stop_update_check();
        }
        if update_all_requested {
            self.start_update_all();
        }
        if let Some(beatmapset_id) = update_single_requested {
            self.start_update_single(beatmapset_id);
        }
        if update_pause_toggled {
            self.toggle_update_pause();
        }
        if update_stop_requested {
            self.stop_update();
        }
        if sign_in_requested {
            self.start_oauth_login();
        }
        if topbar_sign_in_requested {
            self.active_tab = AppTab::Maintenance;
            self.start_oauth_login();
        }
        if sign_out_requested {
            self.sign_out();
        }
        if delete_non_std_requested {
            let selection = DeleteModeSelection {
                taiko: self.delete_taiko,
                catch: self.delete_catch,
                mania: self.delete_mania,
            };
            let count = self.scan.as_ref().map_or(0, |scan| {
                scan.maps
                    .iter()
                    .filter(|map| selection.matches(map.mode))
                    .count()
            });
            self.delete_confirmation = Some(DeleteIntent::NonStdModes(count));
        }

        self.maybe_show_delete_confirmation(ctx);
        self.maybe_show_app_update_window(ctx);
    }
}

/// Rebuilds the selected-hash set after pruning `selected_maps`: kept map
/// hashes plus any still-selected hashes that have no local map (loaded from
/// a collection but absent from the scan). The latter cannot be evaluated by
/// the caller's predicate, so they are preserved rather than silently
/// dropped.
fn reconcile_selected_md5s(
    selected_maps: &[LocalBeatmap],
    missing_hashes: &[String],
    selected_md5s: &BTreeSet<String>,
) -> BTreeSet<String> {
    selected_maps
        .iter()
        .map(|map| map.md5.clone())
        .chain(
            missing_hashes
                .iter()
                .filter(|hash| selected_md5s.contains(*hash))
                .cloned(),
        )
        .collect()
}

fn compact_map_details(map: &LocalBeatmap) -> String {
    let mut details = vec![format!("mapped by {}", map.creator), map.version.clone()];
    if let Some(stars) = map.stars {
        details.push(format!("{}★", format_number(stars)));
    }
    if let Some(bpm) = map.bpm {
        details.push(format!("{} BPM", format_number(bpm)));
    }
    if let Some(length) = map.length_seconds {
        details.push(format_duration(length));
    }
    details.join("  ·  ")
}

fn inspector_grid_value(ui: &mut egui::Ui, label: &str, value: impl std::fmt::Display) {
    ui.label(egui::RichText::new(label).color(egui::Color32::from_rgb(0x9f, 0x9a, 0x93)));
    ui.label(value.to_string());
}

fn optional_number(value: Option<f32>) -> String {
    value.map(format_number).unwrap_or_else(|| "—".to_owned())
}

fn mode_label(mode: Option<u8>) -> &'static str {
    // A missing Mode field means osu!std in the .osu format.
    match mode {
        None | Some(0) => "osu!std",
        Some(1) => "Taiko",
        Some(2) => "Catch",
        Some(3) => "Mania",
        _ => "Unknown",
    }
}

/// Decodes the image picked for the Extras background job on a worker
/// thread. The decode doubles as validation — an image osu! cannot load
/// never reaches the Apply step.
fn load_extras_image(path: &Path) -> Result<ExtrasImage> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let decoded = image::load_from_memory(&bytes).context("decoding the image")?;
    let size = (decoded.width(), decoded.height());
    let preview_image = decoded.thumbnail(1200, 675).to_rgba8();
    let preview = egui::ColorImage::from_rgba_unmultiplied(
        [
            preview_image.width() as usize,
            preview_image.height() as usize,
        ],
        preview_image.as_raw(),
    );
    // The content format decides which target files can take the raw bytes
    // unchanged, so it is sniffed from the bytes rather than the filename.
    let ext = match image::guess_format(&bytes) {
        Ok(image::ImageFormat::Jpeg) => "jpg".to_owned(),
        Ok(image::ImageFormat::Png) => "png".to_owned(),
        Ok(image::ImageFormat::WebP) => "webp".to_owned(),
        Ok(image::ImageFormat::Bmp) => "bmp".to_owned(),
        Ok(_) => {
            return Err(anyhow::anyhow!("unsupported image format"));
        }
        Err(err) => return Err(anyhow::anyhow!("detecting image format: {err}")),
    };
    Ok(ExtrasImage {
        bytes: Arc::new(bytes),
        ext,
        preview,
        size,
    })
}

fn load_background_image(path: &Path) -> Result<egui::ColorImage> {
    if let Some(image) = load_background_jpeg_fast(path)? {
        return Ok(image);
    }

    let image = image::io::Reader::open(path)
        .with_context(|| format!("opening {}", path.display()))?
        .with_guessed_format()
        .with_context(|| format!("detecting image format for {}", path.display()))?
        .decode()
        .with_context(|| format!("decoding {}", path.display()))?
        .thumbnail(1200, 675)
        .to_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    Ok(egui::ColorImage::from_rgba_unmultiplied(
        size,
        image.as_raw(),
    ))
}

fn load_background_jpeg_fast(path: &Path) -> Result<Option<egui::ColorImage>> {
    let mut file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut decoder = jpeg_decoder::Decoder::new(&mut file);
    decoder
        .read_info()
        .with_context(|| format!("reading jpeg header for {}", path.display()))?;

    let Some(info) = decoder.info() else {
        return Ok(None);
    };
    if !matches!(
        info.pixel_format,
        jpeg_decoder::PixelFormat::RGB24 | jpeg_decoder::PixelFormat::L8
    ) {
        return Ok(None);
    }

    // Decodes at a reduced IDCT size (1/8..1) and reports the actual output
    // dimensions, which can differ from the header size above.
    let (scaled_width, scaled_height) = decoder
        .scale(BACKGROUND_PREVIEW_WIDTH, BACKGROUND_PREVIEW_HEIGHT)
        .with_context(|| format!("configuring jpeg scaling for {}", path.display()))?;
    let pixels = decoder
        .decode()
        .with_context(|| format!("decoding {}", path.display()))?;

    let rgb = match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => pixels,
        jpeg_decoder::PixelFormat::L8 => pixels
            .iter()
            .flat_map(|&luminance| [luminance, luminance, luminance])
            .collect(),
        _ => return Ok(None),
    };
    let decoded = image::RgbImage::from_raw(u32::from(scaled_width), u32::from(scaled_height), rgb)
        .with_context(|| format!("decoding {}", path.display()))?;
    let thumbnail = image::DynamicImage::ImageRgb8(decoded)
        .thumbnail(
            u32::from(BACKGROUND_PREVIEW_WIDTH),
            u32::from(BACKGROUND_PREVIEW_HEIGHT),
        )
        .to_rgba8();
    let size = [thumbnail.width() as usize, thumbnail.height() as usize];
    Ok(Some(egui::ColorImage::from_rgba_unmultiplied(
        size,
        thumbnail.as_raw(),
    )))
}

fn ffmpeg_executable() -> PathBuf {
    if let Some(path) = std::env::var_os("FFMPEG_PATH") {
        return PathBuf::from(path);
    }

    if let Ok(current_exe) = std::env::current_exe()
        && let Some(app_dir) = current_exe.parent()
    {
        let executable_name = if cfg!(target_os = "windows") {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        };
        for candidate in [
            app_dir.join(executable_name),
            app_dir.join("tools").join(executable_name),
        ] {
            if candidate.is_file() {
                return candidate;
            }
        }
    }

    PathBuf::from("ffmpeg")
}

fn open_with_default_app(path: &Path) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer.exe")
            .arg(path)
            .spawn()
            .with_context(|| format!("opening {}", path.display()))?;
        Ok(())
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(path)
            .spawn()
            .with_context(|| format!("opening {}", path.display()))?;
        Ok(())
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(path)
            .spawn()
            .with_context(|| format!("opening {}", path.display()))?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct ModeCounts {
    std: usize,
    taiko: usize,
    catch: usize,
    mania: usize,
    unknown: usize,
}

fn count_modes(maps: &[LocalBeatmap]) -> ModeCounts {
    let mut counts = ModeCounts::default();
    for map in maps {
        match map.mode {
            None | Some(0) => counts.std += 1,
            Some(1) => counts.taiko += 1,
            Some(2) => counts.catch += 1,
            Some(3) => counts.mania += 1,
            _ => counts.unknown += 1,
        }
    }
    counts
}

#[derive(Debug, Clone)]
struct DeleteModeSelection {
    taiko: bool,
    catch: bool,
    mania: bool,
}

impl DeleteModeSelection {
    fn any(&self) -> bool {
        self.taiko || self.catch || self.mania
    }

    fn matches(&self, mode: Option<u8>) -> bool {
        match mode {
            Some(1) => self.taiko,
            Some(2) => self.catch,
            Some(3) => self.mania,
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
struct RepairJob {
    beatmapset_id: i64,
    /// "Artist - Title" of the set, from the scan — shown instead of a bare
    /// numeric set id.
    title: Option<String>,
    labels: Vec<String>,
    folders: Vec<PathBuf>,
    issues: Vec<String>,
    /// Asset file names (e.g. `audio.mp3`, `bg.jpg`) that were reported
    /// missing on disk. The repair restores exactly these from the download.
    missing_files: Vec<String>,
    ignore_after_success: Vec<IgnoredRepairIssue>,
}

/// Short chip labels for the active filters, shown under the Filters heading
/// so nothing active can hide inside a collapsed group. Full-range sliders
/// (the defaults) are omitted — they constrain nothing worth announcing.
fn filter_chips(filters: &BeatmapFilters) -> Vec<String> {
    fn narrowed(range: &RangeFilter, full: (f32, f32)) -> bool {
        range.enabled && (range.min > full.0 || range.max < full.1)
    }
    let mut chips = Vec::new();
    if filters.mode != ModeFilter::Any {
        let mode = match filters.mode {
            ModeFilter::Osu => "osu!",
            ModeFilter::Taiko => "taiko",
            ModeFilter::Catch => "catch",
            ModeFilter::Mania => "mania",
            ModeFilter::Any => "any",
        };
        chips.push(format!("mode: {mode}"));
    }
    for (name, range, full) in [
        ("★", &filters.stars, STARS_RANGE),
        ("AR", &filters.ar, AR_RANGE),
        ("CS", &filters.cs, CS_RANGE),
        ("OD", &filters.od, OD_RANGE),
        ("HP", &filters.hp, HP_RANGE),
        ("BPM", &filters.bpm, BPM_RANGE),
    ] {
        if narrowed(range, full) {
            chips.push(format!(
                "{name} {}–{}",
                format_number(range.min),
                format_number(range.max)
            ));
        }
    }
    let min = filters.length_min.trim().parse::<f32>().ok();
    let max = filters.length_max.trim().parse::<f32>().ok();
    if min.is_some() || max.is_some() {
        chips.push(format!(
            "length {}–{}s",
            min.map(format_number).unwrap_or_else(|| "…".to_owned()),
            max.map(format_number).unwrap_or_else(|| "…".to_owned()),
        ));
    }
    for (name, value) in [
        ("artist", &filters.artist),
        ("title", &filters.title),
        ("mapper", &filters.mapper),
        ("difficulty", &filters.difficulty),
        ("tag", &filters.tag),
    ] {
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        let shown = if value.chars().count() > 14 {
            format!("{}…", value.chars().take(13).collect::<String>())
        } else {
            value.to_owned()
        };
        chips.push(format!("{name}: {shown}"));
    }
    chips
}

/// "1 map" / "3 maps" — no "(s)" doc-speak in user-visible strings.
fn plural(count: usize, singular: &str, plural_form: &str) -> String {
    format!(
        "{count} {}",
        if count == 1 { singular } else { plural_form }
    )
}

/// Human-readable values of the enabled numeric filters, shown next to each
/// result so beginners see why a map matched.
fn filter_label_values(filters: &BeatmapFilters, map: &LocalBeatmap) -> Vec<String> {
    let mut fields = Vec::new();
    if filters.stars.enabled {
        fields.extend(map.stars.map(|value| format!("*{}", format_number(value))));
    }
    if filters.ar.enabled {
        fields.extend(map.ar.map(|value| format!("AR {}", format_number(value))));
    }
    if filters.cs.enabled {
        fields.extend(map.cs.map(|value| format!("CS {}", format_number(value))));
    }
    if filters.od.enabled {
        fields.extend(map.od.map(|value| format!("OD {}", format_number(value))));
    }
    if filters.hp.enabled {
        fields.extend(map.hp.map(|value| format!("HP {}", format_number(value))));
    }
    if filters.bpm.enabled {
        fields.extend(map.bpm.map(|value| format!("{} BPM", format_number(value))));
    }
    if !filters.length_min.trim().is_empty() || !filters.length_max.trim().is_empty() {
        fields.extend(
            map.length_seconds
                .map(|value| format!("{} length", format_duration(value))),
        );
    }
    fields
}

fn matches_visible_filters(filters: &BeatmapFilters, map: &LocalBeatmap) -> bool {
    filters.matches_local(map)
}

fn scan_progress_status(scanned_maps: usize, total_maps: usize, matched_maps: usize) -> String {
    format!("Maps read: {scanned_maps}/{total_maps} | Current filters: {matched_maps:>6}")
}

fn apply_theme(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    let bg = egui::Color32::from_rgb(0x13, 0x14, 0x17);
    let panel = egui::Color32::from_rgb(0x1c, 0x1d, 0x21);
    let surface = egui::Color32::from_rgb(0x24, 0x25, 0x2a);
    let surface_hover = egui::Color32::from_rgb(0x2e, 0x2f, 0x35);
    let border = egui::Color32::from_rgb(0x38, 0x39, 0x40);
    let text = egui::Color32::from_rgb(0xea, 0xe6, 0xde);
    let muted = egui::Color32::from_rgb(0xb8, 0xb2, 0xa9);
    // osu!-pink-tinted accent: warm, pleasant, still professional on dark.
    let accent = egui::Color32::from_rgb(0xd8, 0x9a, 0xb0);
    let accent_soft = egui::Color32::from_rgb(0x3a, 0x2b, 0x33);

    style.spacing.item_spacing = egui::vec2(10.0, 8.0);
    style.spacing.button_padding = egui::vec2(12.0, 6.0);
    style.spacing.interact_size = egui::vec2(88.0, 30.0);
    style.visuals = egui::Visuals::dark();
    style.visuals.override_text_color = Some(text);
    style.visuals.panel_fill = bg;
    style.visuals.window_fill = panel;
    style.visuals.extreme_bg_color = bg;
    style.visuals.faint_bg_color = surface;
    style.visuals.code_bg_color = surface;
    style.visuals.hyperlink_color = accent;
    style.visuals.selection.bg_fill = accent_soft;
    style.visuals.selection.stroke = egui::Stroke::new(1.0_f32, text);
    style.visuals.warn_fg_color = egui::Color32::from_rgb(0xc4, 0xa2, 0x6a);
    style.visuals.error_fg_color = egui::Color32::from_rgb(0xc2, 0x6b, 0x72);
    style.visuals.window_rounding = egui::Rounding::same(10.0);
    style.visuals.menu_rounding = egui::Rounding::same(8.0);
    style.visuals.window_stroke = egui::Stroke::new(1.0_f32, border);
    style.visuals.widgets.noninteractive.bg_fill = surface;
    style.visuals.widgets.noninteractive.weak_bg_fill = surface;
    style.visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, border);
    style.visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0_f32, muted);
    style.visuals.widgets.noninteractive.rounding = egui::Rounding::same(8.0);
    style.visuals.widgets.inactive.bg_fill = surface;
    style.visuals.widgets.inactive.weak_bg_fill = surface;
    style.visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0_f32, border);
    style.visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0_f32, text);
    style.visuals.widgets.inactive.rounding = egui::Rounding::same(8.0);
    style.visuals.widgets.hovered.bg_fill = surface_hover;
    style.visuals.widgets.hovered.weak_bg_fill = surface_hover;
    style.visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0_f32, accent);
    style.visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0_f32, text);
    style.visuals.widgets.hovered.rounding = egui::Rounding::same(8.0);
    style.visuals.widgets.active.bg_fill = accent_soft;
    style.visuals.widgets.active.weak_bg_fill = accent_soft;
    style.visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0_f32, accent);
    style.visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0_f32, text);
    style.visuals.widgets.active.rounding = egui::Rounding::same(8.0);
    ctx.set_style(style);
}

fn panel_frame(style: &egui::Style) -> egui::Frame {
    egui::Frame::side_top_panel(style)
        .inner_margin(egui::Margin::symmetric(14.0, 10.0))
        .fill(egui::Color32::from_rgb(0x18, 0x19, 0x1c))
        .stroke(egui::Stroke::new(
            1.0_f32,
            egui::Color32::from_rgb(0x32, 0x33, 0x37),
        ))
}

pub(crate) fn section_frame(style: &egui::Style) -> egui::Frame {
    egui::Frame::group(style)
        .inner_margin(egui::Margin::same(12.0))
        .outer_margin(egui::Margin::same(0.0))
        .rounding(egui::Rounding::same(10.0))
        .fill(egui::Color32::from_rgb(0x20, 0x21, 0x24))
        .stroke(egui::Stroke::new(
            1.0_f32,
            egui::Color32::from_rgb(0x3a, 0x3b, 0x40),
        ))
}

fn inspector_frame(style: &egui::Style) -> egui::Frame {
    egui::Frame::group(style)
        .inner_margin(egui::Margin::same(12.0))
        .outer_margin(egui::Margin::same(0.0))
        .rounding(egui::Rounding::same(8.0))
        .fill(egui::Color32::from_rgb(0x20, 0x21, 0x24))
        .stroke(egui::Stroke::new(
            1.0_f32,
            egui::Color32::from_rgb(0x3a, 0x3b, 0x40),
        ))
}

fn nested_frame(style: &egui::Style) -> egui::Frame {
    egui::Frame::group(style)
        .inner_margin(egui::Margin::same(10.0))
        .rounding(egui::Rounding::same(8.0))
        .fill(egui::Color32::from_rgb(0x26, 0x27, 0x2b))
        .stroke(egui::Stroke::new(
            1.0_f32,
            egui::Color32::from_rgb(0x3a, 0x3b, 0x40),
        ))
}

pub(crate) fn muted_label(ui: &mut egui::Ui, text: impl Into<String>) {
    ui.add(
        egui::Label::new(
            egui::RichText::new(text.into()).color(egui::Color32::from_rgb(0xb3, 0xad, 0xa5)),
        )
        .wrap(true),
    );
}

fn scan_status_label(ui: &mut egui::Ui, prefix: &str, value: &str) {
    let text = format!("{prefix}: {value}");
    let width = ui.available_width().max(1.0);
    ui.add_sized([width, 18.0], egui::Label::new(text.clone()).truncate(true))
        .on_hover_text(text);
}

fn wrapped_label(ui: &mut egui::Ui, text: impl Into<egui::WidgetText>) {
    ui.add(egui::Label::new(text).wrap(true));
}

/// Labelled text box for a word filter. Blank means "anything".
fn filter_text_row(ui: &mut egui::Ui, label: &str, text: &mut String) -> bool {
    ui.label(egui::RichText::new(label).strong());
    ui.add_sized(
        [ui.available_width().max(64.0), 28.0],
        egui::TextEdit::singleline(text).vertical_align(egui::Align::Center),
    )
    .changed()
}

/// Checkbox plus min/max sliders for a numeric filter. `decimals` controls
/// how the picked range is shown (1 for stars/AR/..., 0 for BPM). Returns
/// whether anything changed, so callers persisting on edit can react.
fn filter_range_row(
    ui: &mut egui::Ui,
    label: &str,
    filter: &mut RangeFilter,
    bounds: (f32, f32),
    step: f64,
    decimals: usize,
) -> bool {
    let mut changed = ui
        .horizontal(|ui| {
            let response = ui.checkbox(&mut filter.enabled, "");
            ui.label(egui::RichText::new(label).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(format!(
                    "{:.*} – {:.*}",
                    decimals, filter.min, decimals, filter.max
                ));
            });
            response.changed()
        })
        .inner;
    let previous = (filter.min, filter.max);
    ui.add_enabled_ui(filter.enabled, |ui| {
        if slow_range_row(ui, &mut filter.min, bounds, step, decimals, "Min") {
            changed = true;
        }
    });
    ui.add_enabled_ui(filter.enabled, |ui| {
        if slow_range_row(ui, &mut filter.max, bounds, step, decimals, "Max") {
            changed = true;
        }
    });
    // Keep min <= max, moving the bound the user just dragged.
    if filter.min != previous.0 && filter.min > filter.max {
        filter.max = filter.min;
        changed = true;
    }
    if filter.max != previous.1 && filter.max < filter.min {
        filter.min = filter.max;
        changed = true;
    }
    changed
}

/// One filter bound: caption, exact value box, and a rail. The rail is a
/// relative drag — the handle deliberately lags the cursor at 70% of the
/// cursor travel mapped onto the range, instead of sticking to the pointer.
/// The value box keeps the slow absolute drag from before (Shift = 10x
/// slower) plus arrow-key stepping for exact values.
fn slow_range_row(
    ui: &mut egui::Ui,
    value: &mut f32,
    bounds: (f32, f32),
    step: f64,
    decimals: usize,
    caption: &str,
) -> bool {
    ui.horizontal(|ui| {
        ui.label(caption);
        let drag = ui.add_sized(
            [64.0, 20.0],
            egui::DragValue::new(value)
                .clamp_range(bounds.0..=bounds.1)
                .speed((bounds.1 - bounds.0) as f64 / 1000.0)
                .max_decimals(decimals),
        );
        let rail = relative_rail(ui, value, bounds, step);
        drag.changed() || rail.changed()
    })
    .inner
}

/// Drag state for one rail, keyed by widget id. Travel is accumulated (in
/// units of rail widths) instead of applied per frame, so a single glitch
/// frame — pointer warp, hitch, coalesced events — can never teleport the
/// value: per-frame travel is clamped and the total always equals pointer
/// travel times the speed factor.
#[derive(Debug, Clone, Copy)]
struct RailDrag {
    start_value: f32,
    travel: f32,
    applied_value: f32,
}

fn relative_rail(
    ui: &mut egui::Ui,
    value: &mut f32,
    bounds: (f32, f32),
    step: f64,
) -> egui::Response {
    const SPEED_FACTOR: f32 = 0.7;
    /// No single frame may move the value more than this fraction of the
    /// range, however large its pointer delta claims to be.
    const MAX_FRAME_TRAVEL: f32 = 0.35;

    let (min, max) = bounds;
    let span = (max - min).max(f32::EPSILON);
    let (rect, mut response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width().max(40.0), 20.0),
        egui::Sense::drag(),
    );
    response = response.on_hover_and_drag_cursor(egui::CursorIcon::ResizeHorizontal);
    let id = response.id;

    if response.drag_started() {
        ui.data_mut(|data| {
            data.insert_temp(
                id,
                RailDrag {
                    start_value: *value,
                    travel: 0.0,
                    applied_value: *value,
                },
            )
        });
    }
    if response.dragged() {
        let frame_travel = (response.drag_delta().x / rect.width().max(1.0))
            .clamp(-MAX_FRAME_TRAVEL, MAX_FRAME_TRAVEL);
        let mut drag: RailDrag = ui.data_mut(|data| data.get_temp(id)).unwrap_or(RailDrag {
            start_value: *value,
            travel: 0.0,
            applied_value: *value,
        });
        // Another control (e.g. min/max coupling) may have moved the value
        // mid-drag: rebase instead of fighting it.
        if *value != drag.applied_value {
            drag.start_value = *value;
            drag.travel = 0.0;
        }
        drag.travel += frame_travel;
        let mut next = (drag.start_value + drag.travel * span * SPEED_FACTOR).clamp(min, max);
        if step > 0.0 {
            let step = step as f32;
            next = ((next / step).round() * step).clamp(min, max);
        }
        drag.applied_value = next;
        ui.data_mut(|data| data.insert_temp(id, drag));
        if next != *value {
            *value = next;
            response.mark_changed();
        }
    }
    if response.drag_stopped() {
        ui.data_mut(|data| data.remove::<RailDrag>(id));
    }

    if ui.is_rect_visible(rect) {
        let visuals = ui.style().interact(&response);
        let center_y = rect.center().y;
        ui.painter().rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(rect.left(), center_y - 3.0),
                egui::pos2(rect.right(), center_y + 3.0),
            ),
            3.0,
            visuals.bg_fill,
        );
        let t = (*value - min) / span;
        ui.painter().circle_filled(
            egui::pos2(rect.left() + t * rect.width(), center_y),
            7.0,
            visuals.fg_stroke.color,
        );
    }
    response
}

fn fix_ui_width(ui: &mut egui::Ui, width: f32) {
    let width = width.max(1.0);
    ui.set_width(width);
    ui.set_min_width(width);
    ui.set_max_width(width);
}

fn fill_tile_width(ui: &mut egui::Ui) {
    fix_ui_width(ui, ui.available_width());
}

#[allow(clippy::too_many_arguments)]
fn status_log_label(
    ui: &mut egui::Ui,
    status: RepairLogStatus,
    in_progress: &'static str,
    success: &'static str,
    failed: &'static str,
    skipped: &'static str,
    beatmapset_id: i64,
    message: &str,
) {
    let (label, color) = match status {
        RepairLogStatus::InProgress => (in_progress, None),
        RepairLogStatus::Success => (success, Some(egui::Color32::from_rgb(0x7f, 0xa6, 0x86))),
        RepairLogStatus::Failed => (failed, Some(egui::Color32::from_rgb(0xc2, 0x6b, 0x72))),
        RepairLogStatus::Skipped => (skipped, None),
    };
    let number_color = egui::Color32::from_rgb(0xb0, 0x9d, 0x7d);

    ui.horizontal_wrapped(|ui| {
        let mut rich = egui::RichText::new(format!("{label}:")).strong();
        if let Some(color) = color {
            rich = rich.color(color);
        }
        ui.label(rich);
        // A zero id marks an aggregate entry that spans many sets; there is
        // no single set number to show.
        if beatmapset_id != 0 {
            ui.label("set");
            ui.label(
                egui::RichText::new(beatmapset_id.to_string())
                    .color(number_color)
                    .strong(),
            );
            ui.label("-");
        }
        colored_number_text(ui, message, number_color);
    });
}

fn colored_number_text(ui: &mut egui::Ui, text: &str, number_color: egui::Color32) {
    let mut chunk = String::new();
    let mut chunk_is_number = false;

    for ch in text.chars() {
        let is_number = ch.is_ascii_digit();
        if !chunk.is_empty() && is_number != chunk_is_number {
            add_number_text_chunk(ui, &chunk, chunk_is_number, number_color);
            chunk.clear();
        }
        chunk.push(ch);
        chunk_is_number = is_number;
    }

    if !chunk.is_empty() {
        add_number_text_chunk(ui, &chunk, chunk_is_number, number_color);
    }
}

fn add_number_text_chunk(
    ui: &mut egui::Ui,
    text: &str,
    is_number: bool,
    number_color: egui::Color32,
) {
    if is_number {
        ui.label(egui::RichText::new(text).color(number_color).strong());
    } else {
        ui.label(text);
    }
}

/// Removes folders left completely empty by file deletions. Only fully empty
/// directories are removed — anything still holding audio, images, or other
/// maps is left alone. Returns how many folders were removed.
fn remove_empty_folders(folders: impl IntoIterator<Item = PathBuf>) -> usize {
    let unique = folders.into_iter().collect::<BTreeSet<_>>();
    let mut removed = 0;
    for folder in unique {
        let is_empty = fs::read_dir(&folder)
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false);
        if is_empty && fs::remove_dir(&folder).is_ok() {
            removed += 1;
        }
    }
    removed
}

fn build_sets_for_scan(maps: &[LocalBeatmap]) -> Vec<LocalBeatmapSet> {
    let mut grouped = BTreeMap::<(Option<i64>, PathBuf), Vec<LocalBeatmap>>::new();
    for map in maps {
        grouped
            .entry((map.beatmapset_id, map.folder.clone()))
            .or_default()
            .push(map.clone());
    }
    grouped
        .into_iter()
        .map(|((beatmapset_id, folder), maps)| LocalBeatmapSet {
            beatmapset_id,
            folder,
            maps,
        })
        .collect()
}

fn format_number(value: f32) -> String {
    let rounded = (value * 100.0).round() / 100.0;
    if (rounded.fract()).abs() < f32::EPSILON {
        format!("{rounded:.0}")
    } else {
        format!("{rounded:.2}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_owned()
    }
}

fn format_duration(seconds: f32) -> String {
    let total = seconds.round().max(0.0) as u32;
    format!("{}:{:02}", total / 60, total % 60)
}

/// Whether an issue is about a missing background (as opposed to missing
/// audio or a parse warning). Matches the stable message text so scan caches
/// written before issues carried `missing_file` are covered too.
fn is_missing_background_issue(issue: &local::RepairIssue) -> bool {
    issue
        .message
        .to_ascii_lowercase()
        .contains("missing background file")
}

/// The missing-background opt-out checkbox, shared by the sidebar's Advanced
/// section and the Maintenance card. Updates the persisted ignore store in
/// place; returns true when the value changed and the caller should save it.
fn ignore_backgrounds_checkbox(store: &mut RepairIgnoreStore, ui: &mut egui::Ui) -> bool {
    let mut ignore = store.ignore_missing_backgrounds;
    let changed = ui
        .checkbox(&mut ignore, "Ignore missing backgrounds")
        .changed();
    if changed {
        store.ignore_missing_backgrounds = ignore;
    }
    changed
}

/// Problems left after the missing-background opt-out (see
/// [`RepairIgnoreStore::ignore_missing_backgrounds`]). Every count, list and
/// repair grouping must go through this so the checkbox takes effect
/// everywhere at once, including on already-cached scans.
fn visible_problems(
    problems: &[local::RepairIssue],
    ignore_missing_backgrounds: bool,
) -> impl Iterator<Item = &local::RepairIssue> {
    problems
        .iter()
        .filter(move |issue| !(ignore_missing_backgrounds && is_missing_background_issue(issue)))
}

fn repair_jobs(scan: &LibraryScan, ignore_missing_backgrounds: bool) -> Vec<RepairJob> {
    let corrupted_paths: BTreeSet<_> = visible_problems(&scan.problems, ignore_missing_backgrounds)
        .filter(|issue| issue.severity == RepairSeverity::MissingRequiredFile)
        .map(|issue| issue.beatmap.clone())
        .collect();
    let mut issue_messages = BTreeMap::<PathBuf, Vec<String>>::new();
    let mut issue_missing_files = BTreeMap::<PathBuf, BTreeSet<String>>::new();
    for issue in visible_problems(&scan.problems, ignore_missing_backgrounds)
        .filter(|issue| issue.severity == RepairSeverity::MissingRequiredFile)
    {
        issue_messages
            .entry(issue.beatmap.clone())
            .or_default()
            .push(issue.message.clone());
        if let Some(file) = &issue.missing_file {
            issue_missing_files
                .entry(issue.beatmap.clone())
                .or_default()
                .insert(file.clone());
        }
    }

    let mut grouped = BTreeMap::<
        i64,
        (
            Vec<String>,
            BTreeSet<PathBuf>,
            Vec<String>,
            BTreeSet<String>,
            Vec<IgnoredRepairIssue>,
        ),
    >::new();

    for map in &scan.maps {
        if !corrupted_paths.contains(&map.path) {
            continue;
        }
        if let Some(beatmapset_id) = map.beatmapset_id {
            let entry = grouped.entry(beatmapset_id).or_default();
            entry.0.push(map.label());
            entry.1.insert(map.folder.clone());
            match issue_missing_files.get(&map.path) {
                // The scan already determined which files are missing; reusing
                // the filenames it attached to the issues keeps this rebuild
                // (which used to run once per streamed map event) free of disk
                // access even when every map in the library is flagged.
                Some(files) => entry.3.extend(files.iter().cloned()),
                // Scan caches written before issues carried their filename:
                // re-derive by checking the referenced files on disk.
                None => {
                    for missing in missing_asset_filenames(map, ignore_missing_backgrounds) {
                        entry.3.insert(missing);
                    }
                }
            }
            for message in issue_messages.get(&map.path).into_iter().flatten() {
                let file = map
                    .path
                    .file_name()
                    .and_then(|file| file.to_str())
                    .unwrap_or("unknown .osu");
                entry.2.push(format!("{file}: {message}"));
                if message
                    .to_ascii_lowercase()
                    .contains("missing background file")
                {
                    entry.3.insert(
                        map.background_filename
                            .clone()
                            .unwrap_or_else(|| file.to_owned()),
                    );
                    entry.4.push(IgnoredRepairIssue {
                        beatmapset_id: map.beatmapset_id,
                        beatmap_id: map.beatmap_id,
                        beatmap_file: file.to_owned(),
                        message: message.clone(),
                    });
                }
            }
        }
    }

    let set_titles: std::collections::HashMap<i64, String> = scan
        .sets
        .iter()
        .filter_map(|set| {
            let id = set.beatmapset_id?;
            let first = set.maps.first()?;
            Some((id, format!("{} - {}", first.artist, first.title)))
        })
        .collect();

    grouped
        .into_iter()
        .map(
            |(beatmapset_id, (labels, folders, issues, missing_files, ignore_after_success))| {
                RepairJob {
                    beatmapset_id,
                    title: set_titles.get(&beatmapset_id).cloned(),
                    labels,
                    folders: folders.into_iter().collect(),
                    issues,
                    missing_files: missing_files.into_iter().collect(),
                    ignore_after_success,
                }
            },
        )
        .collect()
}

/// Asset file names missing for this map, derived by checking the map's own
/// audio/background references on disk. Fallback for scan caches written
/// before [`RepairIssue`] carried `missing_file`; current scans reuse the
/// filenames attached to the issues instead of re-statting every flagged file.
/// Backgrounds are skipped when the missing-background opt-out is set.
fn missing_asset_filenames(map: &LocalBeatmap, ignore_missing_backgrounds: bool) -> Vec<String> {
    let mut missing = Vec::new();
    if let Some(audio) = &map.audio_filename
        && !map.folder.join(audio).exists()
    {
        missing.push(audio.clone());
    }
    if !ignore_missing_backgrounds
        && let Some(background) = &map.background_filename
        && !map.folder.join(background).exists()
    {
        missing.push(background.clone());
    }
    missing
}

fn upsert_log(
    log: &mut Vec<RepairLogEntry>,
    beatmapset_id: i64,
    status: RepairLogStatus,
    message: String,
) {
    if let Some(entry) = log
        .iter_mut()
        .find(|entry| entry.beatmapset_id == beatmapset_id)
    {
        entry.status = status;
        entry.message = message;
    } else {
        log.push(RepairLogEntry {
            beatmapset_id,
            status,
            message,
        });
    }
}

fn run_repair_jobs(
    jobs: Vec<RepairJob>,
    backend_url: String,
    osu_root: String,
    mut oauth_session: Option<OauthSession>,
    tx: mpsc::Sender<RepairEvent>,
) {
    let total = jobs.len();
    let _ = tx.send(RepairEvent::Started { total });
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new());

    // Refresh the osu! token once up front so every download in this batch can
    // use the official osu! API. On failure the session is dropped and the
    // downloads fall back to the mirror instead of failing the whole batch.
    if oauth_session.is_some()
        && osu_oauth::ensure_access_token(&client, &backend_url, &mut oauth_session).is_ok()
        && let Some(session) = &oauth_session
    {
        let _ = osu_oauth::save_oauth_session(&osu_root, session);
    }

    for (index, job) in jobs.iter().enumerate() {
        let current = index + 1;
        if index > 0 {
            thread::sleep(BEATMAPSET_DOWNLOAD_DELAY);
        }
        let _ = tx.send(RepairEvent::Opening {
            beatmapset_id: job.beatmapset_id,
            index: current,
            total,
        });
        match repair_beatmapset(&client, job, oauth_session.as_ref()) {
            Ok(outcome) => {
                let _ = tx.send(RepairEvent::Repaired {
                    beatmapset_id: job.beatmapset_id,
                    folders: job.folders.clone(),
                    restored_files: outcome.restored_files,
                    download_source: outcome.download_source,
                    ignored_after_success: job.ignore_after_success.clone(),
                });
            }
            Err(err) => {
                let _ = tx.send(RepairEvent::Failed {
                    beatmapset_id: job.beatmapset_id,
                    message: format!("{err:#}"),
                });
            }
        }
    }

    let _ = tx.send(RepairEvent::Finished);
}

#[allow(clippy::too_many_arguments)]
fn run_update_check(
    targets: Vec<updates::CheckTarget>,
    uncheckable: usize,
    backend_url: String,
    osu_root: String,
    mut oauth_session: Option<OauthSession>,
    tx: mpsc::Sender<UpdateCheckEvent>,
    pause: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
) {
    // osu!'s local database caches each map's rank status, so sets osu! will
    // not publish updates for (ranked/approved/qualified) are dropped before
    // any network request. A missing or unreadable osu!.db only means the
    // online check decides — it still skips frozen statuses after the fetch.
    // The root arrives in the portable display form (`%USERPROFILE%\...`);
    // reading a file needs the expanded real path.
    let osu_root_path = expand_prefilled_path(&osu_root);
    let mut db_index_opt: Option<osu_db::OsuDbIndex> = None;
    let mut db_error = None;
    match osu_db::OsuDbIndex::load(&osu_root_path) {
        Ok(db_index) => db_index_opt = Some(db_index),
        Err(err) => db_error = Some(format!("{err:#}")),
    }
    let mut frozen_count = 0usize;
    let targets: Vec<updates::CheckTarget> = targets
        .into_iter()
        .filter(|target| {
            match local_frozen_status(db_index_opt.as_ref(), &target.locals) {
                // Sets without an online id cannot be pre-filtered; the
                // resolved ones are skipped individually in the loop below.
                Some(_status) if target.beatmapset_id.is_some() => {
                    frozen_count += 1;
                    false
                }
                _ => true,
            }
        })
        .collect();

    let total = targets.len();
    let db_note = match db_error {
        Some(err) => Some(format!(
            "osu!.db unavailable: {err} — every set is checked online"
        )),
        None if frozen_count > 0 => Some(format!(
            "skipped {} ranked/approved/qualified/loved sets via osu!.db",
            frozen_count
        )),
        None => None,
    };
    let _ = tx.send(UpdateCheckEvent::Started {
        total,
        uncheckable,
        db_skipped: frozen_count,
        db_note,
    });
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new());

    // Refresh once up front so the whole check can spend the user's own API
    // quota instead of the Worker's. On failure the session is dropped and
    // the Worker serves its app token under the usual rate limits.
    if oauth_session.is_some()
        && osu_oauth::ensure_access_token(&client, &backend_url, &mut oauth_session).is_ok()
        && let Some(session) = &oauth_session
    {
        let _ = osu_oauth::save_oauth_session(&osu_root, session);
    }
    let access_token = oauth_session
        .as_ref()
        .map(|session| session.access_token.clone());
    // Anonymous checks spend the shared app quota: stay inside osu!'s
    // documented 60 req/min. Signed-in checks spend the user's own quota.
    let request_delay = if access_token.is_some() {
        UPDATE_CHECK_DELAY
    } else {
        UPDATE_CHECK_DELAY_ANONYMOUS
    };

    let mut first_request = true;
    for (position, target) in targets.iter().enumerate() {
        // Pause and stop take effect between sets: the in-flight request
        // always finishes first, the next set waits here.
        if !shrink::wait_while_paused(&pause, &cancel) {
            break;
        }
        let done = position + 1;
        if let Some(set_id) = target.beatmapset_id {
            pace_update_requests(&mut first_request, request_delay);
            check_single_set(
                &client,
                &backend_url,
                access_token.as_deref(),
                set_id,
                &target.locals,
                &tx,
            );
        } else {
            // No set id on file: resolve each beatmap id online, then check
            // the discovered sets.
            match updates::resolve_unknown_sets(
                &client,
                &backend_url,
                access_token.as_deref(),
                &target.locals,
            ) {
                Ok((grouped, _unresolved)) => {
                    for (set_id, resolved) in grouped {
                        if let Some(status) = local_frozen_status(db_index_opt.as_ref(), &resolved)
                        {
                            send_db_skipped(&tx, set_id, status);
                            continue;
                        }
                        pace_update_requests(&mut first_request, request_delay);
                        check_single_set(
                            &client,
                            &backend_url,
                            access_token.as_deref(),
                            set_id,
                            &resolved,
                            &tx,
                        );
                    }
                }
                Err(err) => {
                    let _ = tx.send(UpdateCheckEvent::Unavailable {
                        beatmapset_id: None,
                        reason: format!("resolving beatmap ids failed: {err:#}"),
                    });
                }
            }
        }
        let _ = tx.send(UpdateCheckEvent::Checked { done, total });
    }

    let _ = tx.send(UpdateCheckEvent::Finished);
}

fn pace_update_requests(first_request: &mut bool, delay: Duration) {
    if *first_request {
        *first_request = false;
    } else {
        thread::sleep(delay);
    }
}

/// osu!.db rank status of the first local difficulty whose set the update
/// check skips (ranked/approved/qualified/loved), letting the check skip
/// the set without a network request. `None` means "check online as usual".
fn local_frozen_status(
    db_index: Option<&osu_db::OsuDbIndex>,
    locals: &[updates::LocalDiffRef],
) -> Option<u8> {
    let db_index = db_index?;
    locals.iter().find_map(|local| {
        db_index
            .get(&local.md5, &local.osu_filename)
            .filter(|meta| !updates::db_status_can_receive_updates(meta.ranked_status))
            .map(|meta| meta.ranked_status)
    })
}

fn send_db_skipped(tx: &mpsc::Sender<UpdateCheckEvent>, beatmapset_id: i64, status: u8) {
    let name = osu_db::ranked_status_name(status);
    let _ = tx.send(UpdateCheckEvent::Skipped {
        beatmapset_id,
        reason: format!("{name} (osu!.db) — sets with this status are not checked"),
    });
}

fn check_single_set(
    client: &reqwest::blocking::Client,
    backend_url: &str,
    access_token: Option<&str>,
    beatmapset_id: i64,
    locals: &[updates::LocalDiffRef],
    tx: &mpsc::Sender<UpdateCheckEvent>,
) {
    let folders = locals
        .iter()
        .map(|local| local.folder.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    match updates::fetch_set_meta_blocking(client, backend_url, access_token, beatmapset_id) {
        Err(err) => {
            let _ = tx.send(UpdateCheckEvent::Unavailable {
                beatmapset_id: Some(beatmapset_id),
                reason: format!("{err:#}"),
            });
        }
        Ok(None) => {
            let _ = tx.send(UpdateCheckEvent::Unavailable {
                beatmapset_id: Some(beatmapset_id),
                reason: "no longer available online".to_owned(),
            });
        }
        Ok(Some(remote)) => {
            // Ranked, approved, qualified and loved sets are skipped by
            // policy (see `updates::can_receive_updates`): skip the checksum
            // comparison entirely instead of flagging local drift.
            if !updates::can_receive_updates(&remote.status) {
                let _ = tx.send(UpdateCheckEvent::Skipped {
                    beatmapset_id,
                    reason: format!("{} — sets with this status are not checked", remote.status),
                });
                return;
            }
            let title = format!("{} - {}", remote.artist, remote.title);
            let set = updates::detect_outdated(
                beatmapset_id,
                title,
                remote.last_updated.clone(),
                locals,
                &remote,
                folders,
            );
            if set.outdated_count() > 0 {
                let _ = tx.send(UpdateCheckEvent::Found(set));
            }
        }
    }
}

fn run_update_jobs(
    jobs: Vec<updates::UpdateJob>,
    backend_url: String,
    osu_root: String,
    mut oauth_session: Option<OauthSession>,
    tx: mpsc::Sender<UpdateEvent>,
    pause: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
) {
    let total = jobs.len();
    let _ = tx.send(UpdateEvent::Started { total });
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new());

    if oauth_session.is_some()
        && osu_oauth::ensure_access_token(&client, &backend_url, &mut oauth_session).is_ok()
        && let Some(session) = &oauth_session
    {
        let _ = osu_oauth::save_oauth_session(&osu_root, session);
    }
    let access_token = oauth_session
        .as_ref()
        .map(|session| session.access_token.as_str());

    for (index, job) in jobs.iter().enumerate() {
        // Pause and stop take effect between sets: the in-flight download
        // always finishes first, the next set waits here.
        if !shrink::wait_while_paused(&pause, &cancel) {
            break;
        }
        let current = index + 1;
        if index > 0 {
            thread::sleep(BEATMAPSET_DOWNLOAD_DELAY);
        }
        let _ = tx.send(UpdateEvent::Opening {
            beatmapset_id: job.beatmapset_id,
            index: current,
            total,
        });
        match updates::apply_update_blocking(&client, &backend_url, access_token, job) {
            Ok(outcome) => {
                let _ = tx.send(UpdateEvent::Updated {
                    beatmapset_id: job.beatmapset_id,
                    folders: job.folders.clone(),
                    written: outcome.written_files.len(),
                    removed: outcome.removed_files.len(),
                    download_source: outcome.download_source,
                    checksum_note: outcome.checksum_note,
                });
            }
            Err(err) => {
                let _ = tx.send(UpdateEvent::Failed {
                    beatmapset_id: job.beatmapset_id,
                    message: format!("{err:#}"),
                });
            }
        }
    }

    let _ = tx.send(UpdateEvent::Finished);
}

struct RepairOutcome {
    restored_files: Vec<String>,
    download_source: String,
}

fn repair_beatmapset(
    client: &reqwest::blocking::Client,
    job: &RepairJob,
    oauth_session: Option<&OauthSession>,
) -> Result<RepairOutcome> {
    let temp_path = std::env::temp_dir()
        .join("osu-map-manager-repairs")
        .join(format!("{}.osz", job.beatmapset_id));
    if let Some(parent) = temp_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let access_token = oauth_session.map(|session| session.access_token.as_str());
    let download_source =
        updates::download_beatmapset_file(client, job.beatmapset_id, access_token, &temp_path)?;
    updates::verify_osz(&temp_path)?;
    let mut restored = BTreeSet::new();
    for folder in &job.folders {
        let restored_here = restore_missing_from_osz(&temp_path, folder, &job.missing_files)
            .with_context(|| format!("extracting into {}", folder.display()))?;
        restored.extend(restored_here);
    }
    let restored_files = restored.into_iter().collect::<Vec<_>>();
    verify_repair(job, &restored_files)?;
    Ok(RepairOutcome {
        restored_files,
        download_source,
    })
}

/// Restores only the files flagged as missing (plus any other archive entry
/// whose destination is absent) instead of overwriting the whole song folder,
/// so local scores, edits, and skins are left untouched. Returns the restored
/// file names.
fn restore_missing_from_osz(
    osz_path: &Path,
    folder: &Path,
    missing_files: &[String],
) -> Result<Vec<String>> {
    let wanted = missing_files
        .iter()
        .map(|name| name.to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    let file = fs::File::open(osz_path)?;
    let mut archive = zip::ZipArchive::new(file)?;

    fs::create_dir_all(folder)?;
    let mut restored = Vec::new();
    let mut restored_files = 0_usize;
    let mut restored_bytes = 0_u64;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        if entry.is_dir() {
            continue;
        }
        let Some(enclosed_name) = entry.enclosed_name() else {
            continue;
        };
        let file_name = enclosed_name
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned();
        if file_name.is_empty() {
            continue;
        }
        let output_path = folder.join(enclosed_name);
        let destination_missing = !output_path.exists();
        let explicitly_wanted = wanted.contains(&file_name.to_ascii_lowercase());
        if !explicitly_wanted && !destination_missing {
            continue;
        }
        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent)?;
        }
        // Never clobber an existing file we were not asked to repair.
        if output_path.exists() && !explicitly_wanted {
            continue;
        }
        let mut output = fs::File::create(&output_path)?;
        restored_bytes += io::copy(&mut entry, &mut output)?;
        restored_files += 1;
        updates::check_extract_budget(restored_files, restored_bytes, folder)?;
        restored.push(file_name.to_owned());
    }
    restored.sort();
    restored.dedup();
    Ok(restored)
}

/// Confirms every expected missing file now exists on disk after extraction.
fn verify_repair(job: &RepairJob, restored_files: &[String]) -> Result<()> {
    let restored = restored_files
        .iter()
        .map(|name| name.to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    let mut still_missing = Vec::new();
    for missing in &job.missing_files {
        let present_on_disk = job
            .folders
            .iter()
            .any(|folder| folder.join(missing).exists());
        if !present_on_disk && !restored.contains(&missing.to_ascii_lowercase()) {
            still_missing.push(missing.clone());
        }
    }
    if still_missing.is_empty() {
        return Ok(());
    }
    // Missing entries that are only referenced by stale background ignores are
    // tolerated: the repair still fixed everything else.
    if job.missing_files.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "download did not contain the missing {}: {}",
        if still_missing.len() == 1 {
            "file"
        } else {
            "files"
        },
        still_missing.join(", ")
    )
}

fn load_repair_ignores(osu_root: &str) -> Result<RepairIgnoreStore> {
    let path = repair_ignore_path(osu_root);
    if !path.exists() {
        return Ok(RepairIgnoreStore::default());
    }
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

fn save_repair_ignores(osu_root: &str, store: &RepairIgnoreStore) -> Result<()> {
    let path = repair_ignore_path(osu_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(store)?;
    collection::write_atomic(&path, text.as_bytes())
        .with_context(|| format!("writing {}", path.display()))
}

fn repair_ignore_path(osu_root: &str) -> PathBuf {
    app_data_path(osu_root).join("repair_ignores.json")
}

fn load_scan_cache(osu_root: &str) -> Result<Option<LibraryScan>> {
    let path = scan_cache_path(osu_root);
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let cache = match serde_json::from_str::<ScanCacheFile>(&text) {
        Ok(cache) => cache,
        Err(_) => return Ok(None),
    };
    if cache.version != SCAN_CACHE_VERSION {
        return Ok(None);
    }
    Ok(Some(cache.scan))
}

fn save_scan_cache(osu_root: &str, scan: &LibraryScan) -> Result<()> {
    let path = scan_cache_path(osu_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string(&ScanCacheFile {
        version: SCAN_CACHE_VERSION,
        scan: scan.clone(),
    })?;
    collection::write_atomic(&path, text.as_bytes())
        .with_context(|| format!("writing {}", path.display()))
}

fn scan_cache_path(osu_root: &str) -> PathBuf {
    app_data_path(osu_root).join("scan_cache.json")
}

/// `<osu root>/.osu-map-manager/auto_collections.json`: per-collection
/// auto-add filters. A sidecar because `collection.db` is osu!'s native
/// format and cannot carry extra fields.
fn auto_collections_path(osu_root: &str) -> PathBuf {
    app_data_path(osu_root).join("auto_collections.json")
}

/// The "already had this map" baseline: every non-empty md5 in a scan. An
/// absent baseline (never scanned) means the first completed scan treats
/// every map as newly discovered.
fn scan_md5_baseline(scan: Option<&LibraryScan>) -> HashSet<String> {
    scan.map(|scan| {
        scan.maps
            .iter()
            .filter(|map| !map.md5.is_empty())
            .map(|map| map.md5.clone())
            .collect()
    })
    .unwrap_or_default()
}

/// `<osu root>/Skins` when a root is set, for the shrink tab's skin pass.
fn skins_dir_for(osu_root: &str) -> Option<PathBuf> {
    if osu_root.trim().is_empty() {
        return None;
    }
    Some(expand_prefilled_path(osu_root).join("Skins"))
}

/// `<osu root>/.osu-map-manager/skin_cache.json`: fingerprints of the last
/// skin scan, so unchanged skins load without re-decoding their images.
fn skin_cache_path_for(osu_root: &str) -> Option<PathBuf> {
    if osu_root.trim().is_empty() {
        return None;
    }
    Some(app_data_path(osu_root).join("skin_cache.json"))
}

/// Where the shrink tab persists already-shrunk files.
fn shrink_cache_path(osu_root: &str) -> PathBuf {
    app_data_path(osu_root).join("shrink_cache.json")
}

fn extras_backup_dir(osu_root: &str) -> PathBuf {
    app_data_path(osu_root).join("extras-backups")
}

fn extras_cache_path(osu_root: &str) -> PathBuf {
    app_data_path(osu_root).join("extras_cache.json")
}

/// Summarizes the newest rollback manifest in the backups dir. Manifest
/// names are timestamps, so the lexicographic maximum is the newest apply.
fn read_last_extras_job(osu_root: &str) -> Option<LastExtrasJob> {
    let dir = extras_backup_dir(osu_root);
    let newest = fs::read_dir(&dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .map(|entry| entry.path())
        .max()?;
    let bytes = fs::read(&newest).ok()?;
    let manifest: ExtrasManifest = serde_json::from_slice(&bytes).ok()?;
    Some(LastExtrasJob {
        manifest_path: newest,
        when: manifest.when,
        image_name: manifest.image_name,
        folders: manifest.folders.len(),
    })
}

fn app_data_path(osu_root: &str) -> PathBuf {
    let root = if osu_root.trim().is_empty() {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        expand_prefilled_path(osu_root)
    };
    root.join(".osu-map-manager")
}

/// Derives the osu! install root from a Songs folder path: its parent when
/// the folder itself is named `Songs` (case-insensitive), otherwise the
/// folder itself. Returns `""` when no Songs folder is set.
fn derive_osu_root(songs_dir: &str) -> String {
    if songs_dir.trim().is_empty() {
        return String::new();
    }
    let songs = expand_prefilled_path(songs_dir);
    let is_songs_folder = songs
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("songs"));
    if is_songs_folder {
        let parent = songs.parent().map(PathBuf::from).unwrap_or(songs);
        return display_prefilled_path(&parent);
    }
    display_prefilled_path(&songs)
}

fn default_osu_root() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
        candidates.push(path.join("osu!"));
    }
    if let Some(path) = std::env::var_os("USERPROFILE").map(PathBuf::from) {
        candidates.push(path.join("AppData").join("Local").join("osu!"));
    }

    candidates.into_iter().find(|path| path.exists())
}

fn display_prefilled_path(path: &Path) -> String {
    if let Some(user_profile) = std::env::var_os("USERPROFILE").map(PathBuf::from)
        && let Ok(suffix) = path.strip_prefix(&user_profile)
    {
        let suffix = suffix.display().to_string();
        return if suffix.is_empty() {
            "%USERPROFILE%".to_owned()
        } else {
            format!("%USERPROFILE%\\{suffix}")
        };
    }

    path.display().to_string()
}

fn expand_prefilled_path(path: &str) -> PathBuf {
    let path = path.trim();
    if let Some(user_profile) = std::env::var_os("USERPROFILE")
        && let Some(suffix) = path.strip_prefix("%USERPROFILE%")
    {
        let suffix = suffix.trim_start_matches(['\\', '/']);
        return PathBuf::from(user_profile).join(suffix);
    }
    if let Some(rest) = path.strip_prefix('~')
        && (rest.is_empty() || rest.starts_with(['/', '\\']))
        && let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
    {
        let suffix = rest.trim_start_matches(['\\', '/']);
        return PathBuf::from(home).join(suffix);
    }

    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_test_osz(path: &Path, entries: &[(&str, &[u8])]) {
        let file = fs::File::create(path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        for (name, bytes) in entries {
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            std::io::Write::write_all(&mut writer, bytes).unwrap();
        }
        writer.finish().unwrap();
    }

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
    fn osu_root_derives_from_songs_folder() {
        assert_eq!(derive_osu_root(""), "");
        assert_eq!(
            derive_osu_root("C:\\Games\\osu!\\Songs"),
            "C:\\Games\\osu!".to_owned()
        );
        assert_eq!(
            derive_osu_root("C:\\Games\\osu!\\songs"),
            "C:\\Games\\osu!".to_owned()
        );
        // A folder that is not named `Songs` is its own root.
        assert_eq!(
            derive_osu_root("C:\\Games\\osu!"),
            "C:\\Games\\osu!".to_owned()
        );
    }

    fn drive_rail_frame(
        value: &mut f32,
        rail_rect: &mut egui::Rect,
        ctx: &egui::Context,
        frame_no: u32,
        bounds: (f32, f32),
        events: Vec<egui::Event>,
    ) {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            time: Some(f64::from(frame_no) / 60.0),
            events,
            ..Default::default()
        };
        let _ = ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                *rail_rect = relative_rail(ui, value, bounds, 0.1).rect;
            });
        });
    }

    #[test]
    fn relative_rail_slow_drag_is_smooth_and_bounded() {
        use egui::{Event, Modifiers, PointerButton, Rect, Vec2};

        let ctx = egui::Context::default();
        let mut value = 6.0_f32;
        let bounds = (0.0_f32, 12.0_f32);
        let mut rail_rect = Rect::NOTHING;
        let mut frame_no = 0_u32;
        let mut next_frame = |value: &mut f32, rail_rect: &mut Rect, events: Vec<Event>| {
            frame_no += 1;
            drive_rail_frame(value, rail_rect, &ctx, frame_no, bounds, events);
        };

        // Layout frame: no input, just learn where the rail is.
        next_frame(&mut value, &mut rail_rect, vec![]);
        assert!(rail_rect.width() > 100.0);
        let press_pos = rail_rect.center();

        // Press on the rail.
        next_frame(
            &mut value,
            &mut rail_rect,
            vec![Event::PointerButton {
                pos: press_pos,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::default(),
            }],
        );
        assert_eq!(value, 6.0);

        // Slow drag: 2px per frame. Every frame must move a little — never a
        // multi-unit jump.
        let mut previous = value;
        for step in 1..=60 {
            next_frame(
                &mut value,
                &mut rail_rect,
                vec![Event::PointerMoved(
                    press_pos + Vec2::new(step as f32 * 2.0, 0.0),
                )],
            );
            let delta = (value - previous).abs();
            assert!(delta <= 3.1, "frame {step}: jumped {delta}");
            assert!((0.0..=12.0).contains(&value));
            previous = value;
        }
        // Total applied travel tracks pointer travel times the 0.7 factor.
        let expected = 6.0 + (120.0 / rail_rect.width()) * 12.0 * 0.7;
        assert!(
            (value - expected).abs() < 0.2,
            "value {value}, expected ~{expected}"
        );

        // Teleport frame: 500px in one frame must be clamped, not applied.
        let before = value;
        next_frame(
            &mut value,
            &mut rail_rect,
            vec![Event::PointerMoved(press_pos + Vec2::new(620.0, 0.0))],
        );
        assert!(
            (value - before).abs() <= 3.1,
            "teleport jumped {}",
            (value - before).abs()
        );

        // Release: value stays put.
        next_frame(
            &mut value,
            &mut rail_rect,
            vec![Event::PointerButton {
                pos: press_pos,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::default(),
            }],
        );
        assert!((0.0..=12.0).contains(&value));
    }

    #[test]
    fn only_fully_empty_folders_are_removed() {
        let root = unique_temp_dir("osu-empty-folders");
        let empty = root.join("empty-set");
        let kept = root.join("has-audio");
        fs::create_dir_all(&empty).unwrap();
        fs::create_dir_all(&kept).unwrap();
        fs::write(kept.join("audio.mp3"), b"audio").unwrap();

        let removed = remove_empty_folders(vec![empty.clone(), kept.clone(), root.join("gone")]);

        assert_eq!(removed, 1);
        assert!(!empty.exists());
        assert!(kept.exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn selected_missing_hashes_survive_pruning() {
        let map = |md5: &str| LocalBeatmap {
            md5: md5.to_owned(),
            ..Default::default()
        };
        // Simulates `retain_selected_maps` keeping only "kept".
        let retained = vec![map("kept")];
        let missing = vec![
            "missing-selected".to_owned(),
            "missing-unselected".to_owned(),
        ];
        let selected: BTreeSet<String> = ["kept", "dropped", "missing-selected"]
            .iter()
            .map(|hash| hash.to_string())
            .collect();

        let reconciled = reconcile_selected_md5s(&retained, &missing, &selected);

        assert!(reconciled.contains("kept"));
        // Still selected but absent from the scan: preserved.
        assert!(reconciled.contains("missing-selected"));
        // Pruned map / never selected: gone.
        assert!(!reconciled.contains("dropped"));
        assert!(!reconciled.contains("missing-unselected"));
    }

    #[test]
    fn repair_restores_only_missing_files_without_clobbering() {
        let root = unique_temp_dir("osu-repair-restore");
        let osz = root.join("123.osz");
        write_test_osz(
            &osz,
            &[
                ("audio.mp3", b"audio-bytes"),
                ("bg.jpg", b"bg-bytes"),
                ("existing.osu", b"new-osu-bytes"),
            ],
        );
        let folder = root.join("song-folder");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("existing.osu"), b"local-osu-bytes").unwrap();

        let restored = restore_missing_from_osz(&osz, &folder, &["audio.mp3".to_owned()]).unwrap();

        // The requested file plus any other archive entry absent on disk.
        assert_eq!(restored, vec!["audio.mp3".to_owned(), "bg.jpg".to_owned()]);
        assert_eq!(fs::read(folder.join("audio.mp3")).unwrap(), b"audio-bytes");
        assert_eq!(fs::read(folder.join("bg.jpg")).unwrap(), b"bg-bytes");
        // Present files are left untouched even when the archive has them.
        assert_eq!(
            fs::read(folder.join("existing.osu")).unwrap(),
            b"local-osu-bytes"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn repair_rejects_non_archive_downloads() {
        let root = unique_temp_dir("osu-repair-verify");
        let bad = root.join("bad.osz");
        fs::write(&bad, b"not a zip file at all, just an error page...").unwrap();
        assert!(updates::verify_osz(&bad).is_err());

        let tiny = root.join("tiny.osz");
        fs::write(&tiny, b"PK").unwrap();
        assert!(updates::verify_osz(&tiny).is_err());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn jpeg_fast_path_matches_scaled_output_size() {
        use image::ImageEncoder;

        let root = unique_temp_dir("osu-bg-jpeg");
        // Large enough that the decoder's reduced IDCT size kicks in, which
        // used to mismatch the reported image dimensions.
        let (width, height) = (2000_u32, 1500_u32);
        let rgb = vec![128_u8; (width * height * 3) as usize];
        let path = root.join("bg.jpg");
        let file = fs::File::create(&path).unwrap();
        image::codecs::jpeg::JpegEncoder::new(file)
            .write_image(&rgb, width, height, image::ColorType::Rgb8)
            .unwrap();

        let image = load_background_image(&path).unwrap();
        assert!(image.width() <= BACKGROUND_PREVIEW_WIDTH as usize);
        assert!(image.height() <= BACKGROUND_PREVIEW_HEIGHT as usize);
        assert!(!image.pixels.is_empty());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn mode_counts_treat_missing_mode_as_std() {
        let map_with = |mode: Option<u8>| LocalBeatmap {
            path: Default::default(),
            folder: Default::default(),
            md5: Default::default(),
            beatmap_id: None,
            beatmapset_id: None,
            artist: String::new(),
            title: String::new(),
            source: String::new(),
            creator: String::new(),
            version: String::new(),
            tags: String::new(),
            audio_filename: None,
            background_filename: None,
            mode,
            has_mode_field: mode.is_some(),
            ar: None,
            cs: None,
            od: None,
            hp: None,
            stars: None,
            bpm: None,
            length_seconds: None,
            circles: 0,
            sliders: 0,
        };
        let maps = vec![
            map_with(None),
            map_with(Some(0)),
            map_with(Some(1)),
            map_with(Some(3)),
            map_with(Some(9)),
        ];
        let counts = count_modes(&maps);
        assert_eq!(counts.std, 2);
        assert_eq!(counts.taiko, 1);
        assert_eq!(counts.mania, 1);
        assert_eq!(counts.unknown, 1);
        assert_eq!(mode_label(None), "osu!std");
    }

    #[test]
    fn repair_reports_files_absent_from_download() {
        let root = unique_temp_dir("osu-repair-missing");
        let folder = root.join("song-folder");
        fs::create_dir_all(&folder).unwrap();
        let job = RepairJob {
            beatmapset_id: 42,
            title: None,
            labels: Vec::new(),
            folders: vec![folder],
            issues: Vec::new(),
            missing_files: vec!["gone.mp3".to_owned()],
            ignore_after_success: Vec::new(),
        };
        let err = verify_repair(&job, &[]).unwrap_err();
        assert!(err.to_string().contains("gone.mp3"));

        let _ = fs::remove_dir_all(root);
    }

    fn flagged_scan(folder: &Path, osu_path: &Path, missing_file: Option<String>) -> LibraryScan {
        let map = LocalBeatmap {
            path: osu_path.to_owned(),
            folder: folder.to_owned(),
            md5: "map-md5".into(),
            beatmapset_id: Some(12345),
            background_filename: Some("bg.jpg".into()),
            ..Default::default()
        };
        LibraryScan {
            maps: vec![map],
            problems: vec![local::RepairIssue {
                beatmap: osu_path.to_owned(),
                message: "Missing background file: bg.jpg".into(),
                severity: RepairSeverity::MissingRequiredFile,
                missing_file,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn repair_jobs_reuse_issue_carried_filenames_without_disk_checks() {
        // The background exists on disk here; the issue still claims it was
        // missing at scan time. Grouping must trust the scan's verdict (the
        // disk-stating fallback would find nothing to restore).
        let root = unique_temp_dir("osu-repair-carried");
        let folder = root.join("12345 (Artist - Title)");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("bg.jpg"), b"jpg").unwrap();
        let osu_path = folder.join("Artist - Title (Mapper) [Normal].osu");
        fs::write(&osu_path, b"osu file format v14").unwrap();

        let jobs = repair_jobs(
            &flagged_scan(&folder, &osu_path, Some("bg.jpg".into())),
            false,
        );

        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].beatmapset_id, 12345);
        assert_eq!(jobs[0].missing_files, vec!["bg.jpg".to_owned()]);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn repair_jobs_fall_back_to_disk_for_caches_without_carried_filenames() {
        // Scan caches written before issues carried their filename: the file
        // really is gone, and the stat-based fallback must recover its name.
        let root = unique_temp_dir("osu-repair-fallback");
        let folder = root.join("12345 (Artist - Title)");
        fs::create_dir_all(&folder).unwrap();
        let osu_path = folder.join("Artist - Title (Mapper) [Normal].osu");
        fs::write(&osu_path, b"osu file format v14").unwrap();

        let jobs = repair_jobs(&flagged_scan(&folder, &osu_path, None), false);

        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].missing_files, vec!["bg.jpg".to_owned()]);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn ignoring_backgrounds_hides_those_issues_from_repair_grouping() {
        let root = unique_temp_dir("osu-repair-ignore-bg");
        let folder = root.join("12345 (Artist - Title)");
        fs::create_dir_all(&folder).unwrap();
        let osu_path = folder.join("Artist - Title (Mapper) [Normal].osu");
        fs::write(&osu_path, b"osu file format v14").unwrap();

        // Background-only issue: with the opt-out on there is nothing to
        // repair, so no job is built at all.
        let background_only = flagged_scan(&folder, &osu_path, Some("bg.jpg".into()));
        assert!(repair_jobs(&background_only, true).is_empty());
        assert_eq!(repair_jobs(&background_only, false).len(), 1);

        // Audio + background missing on one map: the job keeps only the audio.
        let map = LocalBeatmap {
            path: osu_path.clone(),
            folder: folder.clone(),
            md5: "map-md5".into(),
            beatmapset_id: Some(12345),
            audio_filename: Some("audio.mp3".into()),
            background_filename: Some("bg.jpg".into()),
            ..Default::default()
        };
        let scan = LibraryScan {
            maps: vec![map],
            problems: vec![
                local::RepairIssue {
                    beatmap: osu_path.clone(),
                    message: "Missing audio file: audio.mp3".into(),
                    severity: RepairSeverity::MissingRequiredFile,
                    missing_file: Some("audio.mp3".into()),
                },
                local::RepairIssue {
                    beatmap: osu_path.clone(),
                    message: "Missing background file: bg.jpg".into(),
                    severity: RepairSeverity::MissingRequiredFile,
                    missing_file: Some("bg.jpg".into()),
                },
            ],
            ..Default::default()
        };
        let jobs = repair_jobs(&scan, true);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].missing_files, vec!["audio.mp3".to_owned()]);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn visible_problems_drop_only_background_issues() {
        let problems = vec![
            local::RepairIssue {
                beatmap: PathBuf::from("a.osu"),
                message: "Missing audio file: audio.mp3".into(),
                severity: RepairSeverity::MissingRequiredFile,
                missing_file: Some("audio.mp3".into()),
            },
            local::RepairIssue {
                beatmap: PathBuf::from("b.osu"),
                message: "Missing background file: bg.jpg".into(),
                severity: RepairSeverity::MissingRequiredFile,
                missing_file: Some("bg.jpg".into()),
            },
            local::RepairIssue {
                beatmap: PathBuf::from("c.osu"),
                message: "Timed out parsing map file after 8 seconds".into(),
                severity: RepairSeverity::ParseWarning,
                missing_file: None,
            },
        ];

        assert_eq!(visible_problems(&problems, false).count(), 3);
        let visible: Vec<_> = visible_problems(&problems, true)
            .map(|issue| issue.message.clone())
            .collect();
        assert_eq!(
            visible,
            vec![
                "Missing audio file: audio.mp3".to_owned(),
                "Timed out parsing map file after 8 seconds".to_owned()
            ]
        );
    }

    #[test]
    fn ignore_store_defaults_and_round_trips_background_flag() {
        // Old repair_ignores.json without the flag keeps loading.
        let old: RepairIgnoreStore = serde_json::from_str(r#"{"entries":[]}"#).unwrap();
        assert!(!old.ignore_missing_backgrounds);

        let store = RepairIgnoreStore {
            ignore_missing_backgrounds: true,
            ..Default::default()
        };
        let text = serde_json::to_string(&store).unwrap();
        let parsed: RepairIgnoreStore = serde_json::from_str(&text).unwrap();
        assert!(parsed.ignore_missing_backgrounds);
    }
}
