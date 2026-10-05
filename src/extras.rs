//! Bulk tools that apply one change across every scanned beatmapset. The
//! first one replaces backgrounds: the content of every background image the
//! scanned charts reference is overwritten with the imported image, encoded
//! to match each file's extension. Chart files (`.osu`/`.osb`) are only ever
//! *read* — they are never written, so checksums and scores are unaffected.
//! Every apply writes a rollback manifest (plus backup copies of the
//! replaced images), so the original contents can be restored later.

use anyhow::{Context, Result, bail, ensure};
use image::ImageEncoder;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
    time::Instant,
};

/// Image extensions osu! loads for backgrounds. The file dialog, the
/// per-file encoding dispatch and the chart parsing all agree on this list.
pub const SUPPORTED_EXTS: &[&str] = &["jpg", "jpeg", "png", "webp", "bmp"];

/// Background images wider than this are downscaled when they have to be
/// re-encoded, mirroring the Shrink tab's rule. The raw bytes pass through
/// untouched when the imported format already matches the target file.
const IMAGE_MAX_WIDTH: u32 = 1920;

/// JPEG quality used when a target file has to be re-encoded.
const JPEG_QUALITY: u8 = 90;
/// Version of the rollback manifest format.
const MANIFEST_VERSION: u32 = 2;
/// Extension alias folded into `jpg` before any dispatch.
const JPEG_ALIAS: &str = "jpeg";

#[derive(Debug)]
pub enum ExtrasEvent {
    Started {
        folders: usize,
    },
    FolderDone {
        folder: PathBuf,
        files: usize,
        cached: usize,
        skipped: usize,
    },
    FolderFailed {
        folder: PathBuf,
        message: String,
    },
    /// A fatal problem before any folder was touched (undecodable image).
    Failed {
        message: String,
    },
    Finished {
        files: usize,
        elapsed_s: f64,
    },
}

#[derive(Debug)]
pub enum RollbackEvent {
    Started {
        folders: usize,
    },
    FolderDone {
        folder: PathBuf,
        restored: usize,
    },
    FolderFailed {
        folder: PathBuf,
        message: String,
    },
    /// The manifest could not be read; nothing was touched.
    Failed {
        message: String,
    },
    /// `cleaned` is true when every folder was restored and the manifest
    /// plus backup files were removed.
    Finished {
        restored: usize,
        cleaned: bool,
        elapsed_s: f64,
    },
}

/// One apply job's rollback record: every background file whose content the
/// job changed, per folder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtrasManifest {
    pub version: u32,
    pub when: String,
    /// Display name of the imported image.
    pub image_name: String,
    pub folders: Vec<FolderBackup>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FolderBackup {
    pub folder: PathBuf,
    /// Backup subdirectory inside the job's backup dir holding the original
    /// image contents.
    pub dir: String,
    /// Background files whose (backed-up) content was replaced.
    pub replaced: Vec<String>,
    /// Background files the job created because a chart referenced a file
    /// that did not exist. Rollback removes them again.
    pub created: Vec<String>,
}

pub const EXTRAS_CACHE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExtrasCacheEntry {
    /// Fingerprint of the image this file was written with; a later apply
    /// of a different image must not skip the file.
    image: String,
    /// File length and mtime (seconds) right after the job wrote it. Any
    /// change to either — an outside edit, or a rollback restoring the
    /// original — invalidates the entry, failing safe towards re-applying.
    len: u64,
    mtime: u64,
}

/// Persistent record of background files that already contain the imported
/// image, so a repeated apply skips them instead of rewriting (and
/// re-backing-up) every file again. Mirrors the shrink cache: entries only
/// count while the file still has the exact length and mtime it had when
/// the job wrote it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtrasCache {
    version: u32,
    entries: HashMap<PathBuf, ExtrasCacheEntry>,
}

impl ExtrasCache {
    pub fn load(path: &Path) -> Self {
        let text = fs::read_to_string(path).unwrap_or_default();
        match serde_json::from_str::<ExtrasCache>(&text) {
            Ok(cache) if cache.version == EXTRAS_CACHE_VERSION => cache,
            _ => ExtrasCache {
                version: EXTRAS_CACHE_VERSION,
                entries: HashMap::new(),
            },
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)?;
        fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// True when `path` currently looks exactly like the file this tool left
    /// behind after writing `image` into it — so it can be skipped.
    fn is_fresh(&self, path: &Path, image: &str) -> bool {
        self.entries.get(path).is_some_and(|entry| {
            entry.image == image
                && file_sig(path)
                    .is_some_and(|(len, mtime)| len == entry.len && mtime == entry.mtime)
        })
    }

    fn record(&mut self, path: &Path, image: &str) {
        if let Some((len, mtime)) = file_sig(path) {
            self.entries.insert(
                path.to_owned(),
                ExtrasCacheEntry {
                    image: image.to_owned(),
                    len,
                    mtime,
                },
            );
        }
    }
}

/// `(len, mtime seconds)` of a file, `None` when it cannot be statted —
/// callers must fail safe (replace, don't skip).
fn file_sig(path: &Path) -> Option<(u64, u64)> {
    let meta = fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some((meta.len(), mtime))
}

/// Stable fingerprint of the imported image, so a cache entry can tell
/// "written with this image" apart from "written with a different one".
fn image_fingerprint(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Replaces the background image contents in every folder and writes the
/// rollback manifest. Sequential on purpose: the work is one image encode
/// per distinct extension plus a file copy per background, so parallelism
/// only adds disk contention, and stopping stays instant.
#[allow(clippy::too_many_arguments)]
pub fn run_background_jobs(
    folders: Vec<PathBuf>,
    image: Arc<Vec<u8>>,
    image_ext: String,
    image_name: String,
    cancel: Arc<AtomicBool>,
    tx: Sender<ExtrasEvent>,
    job_backup_dir: PathBuf,
    manifest_path: PathBuf,
    cache_file: PathBuf,
) {
    let started = Instant::now();
    let _ = tx.send(ExtrasEvent::Started {
        folders: folders.len(),
    });

    // Decoding up front doubles as validation: an image osu! could not load
    // aborts the job before any folder is touched.
    let decoded = match image::load_from_memory(&image) {
        Ok(decoded) => decoded,
        Err(err) => {
            let _ = tx.send(ExtrasEvent::Failed {
                message: format!("decoding the imported image: {err}"),
            });
            let _ = tx.send(ExtrasEvent::Finished {
                files: 0,
                elapsed_s: started.elapsed().as_secs_f64(),
            });
            return;
        }
    };
    let fingerprint = image_fingerprint(&image);
    let mut cache = ExtrasCache::load(&cache_file);
    let payload = BackgroundPayload {
        source_ext: normalize_ext(&image_ext),
        source_bytes: image,
        decoded,
    };

    let mut used_dir_names: HashSet<String> = HashSet::new();
    let mut records: Vec<FolderBackup> = Vec::new();
    let mut files = 0;
    let mut encodings: HashMap<String, Arc<Vec<u8>>> = HashMap::new();
    for folder in folders {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let dir_name = backup_dir_name(&folder, &mut used_dir_names);
        let folder_backup_dir = job_backup_dir.join(&dir_name);
        match apply_to_folder(
            &folder,
            &payload,
            &fingerprint,
            &mut cache,
            &mut encodings,
            &cancel,
            &folder_backup_dir,
        ) {
            Ok(report) => {
                let changed = report.replaced.len() + report.created.len();
                files += changed;
                if changed > 0 {
                    records.push(FolderBackup {
                        folder: folder.clone(),
                        dir: dir_name,
                        replaced: report.replaced,
                        created: report.created,
                    });
                }
                let _ = tx.send(ExtrasEvent::FolderDone {
                    folder,
                    files: changed,
                    cached: report.cached,
                    skipped: report.skipped,
                });
            }
            // A cancel mid-folder surfaces as an error; swallow it here so
            // the half-finished folder is not reported as a failure.
            Err(_) if cancel.load(Ordering::Relaxed) => break,
            Err(err) => {
                let _ = tx.send(ExtrasEvent::FolderFailed {
                    folder,
                    message: format!("{err:#}"),
                });
            }
        }
    }

    // A failed cache save only costs speed on the next run, never
    // correctness, so it is not worth an error event.
    let _ = cache.save(&cache_file);

    // Folders processed before a cancel were modified on disk, so their
    // records must survive: write the manifest even for a partial run.
    if !records.is_empty() {
        let manifest = ExtrasManifest {
            version: MANIFEST_VERSION,
            when: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            image_name,
            folders: records,
        };
        if let Err(err) = write_manifest(&manifest, &manifest_path) {
            let _ = tx.send(ExtrasEvent::Failed {
                message: format!("rollback manifest could not be written: {err:#}"),
            });
        }
    }

    let _ = tx.send(ExtrasEvent::Finished {
        files,
        elapsed_s: started.elapsed().as_secs_f64(),
    });
}

/// Restores the state recorded by one apply job: replaced background images
/// get their original content back, and images the job created (charts
/// referencing files that were missing) are removed. When every folder
/// succeeds, the manifest and backup files are cleaned up.
pub fn run_rollback_job(
    manifest_path: PathBuf,
    cancel: Arc<AtomicBool>,
    tx: Sender<RollbackEvent>,
) {
    let started = Instant::now();
    let load = || -> Result<ExtrasManifest> {
        let bytes = fs::read(&manifest_path)
            .with_context(|| format!("reading {}", manifest_path.display()))?;
        serde_json::from_slice(&bytes).context("parsing the rollback manifest")
    };
    let manifest = match load() {
        Ok(manifest) => manifest,
        Err(err) => {
            let _ = tx.send(RollbackEvent::Failed {
                message: format!("{err:#}"),
            });
            let _ = tx.send(RollbackEvent::Finished {
                restored: 0,
                cleaned: false,
                elapsed_s: started.elapsed().as_secs_f64(),
            });
            return;
        }
    };
    // The job's backup dir sits beside the manifest and shares its stem.
    let job_backup_dir = manifest_path.with_extension("");
    let _ = tx.send(RollbackEvent::Started {
        folders: manifest.folders.len(),
    });

    let mut restored_total = 0;
    let mut failures = 0;
    for record in &manifest.folders {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let folder_backup_dir = job_backup_dir.join(&record.dir);
        match rollback_folder(record, &folder_backup_dir, &cancel) {
            Ok(restored) => {
                restored_total += restored;
                let _ = tx.send(RollbackEvent::FolderDone {
                    folder: record.folder.clone(),
                    restored,
                });
            }
            Err(_) if cancel.load(Ordering::Relaxed) => break,
            Err(err) => {
                failures += 1;
                let _ = tx.send(RollbackEvent::FolderFailed {
                    folder: record.folder.clone(),
                    message: format!("{err:#}"),
                });
            }
        }
    }

    let cleaned = failures == 0 && !cancel.load(Ordering::Relaxed);
    if cleaned {
        // Leftover backup files are harmless: the next rollback just sees
        // this job again, so a failed cleanup is not worth reporting.
        let _ = fs::remove_file(&manifest_path);
        let _ = fs::remove_dir_all(&job_backup_dir);
    }
    let _ = tx.send(RollbackEvent::Finished {
        restored: restored_total,
        cleaned,
        elapsed_s: started.elapsed().as_secs_f64(),
    });
}

fn rollback_folder(
    record: &FolderBackup,
    folder_backup_dir: &Path,
    cancel: &AtomicBool,
) -> Result<usize> {
    let folder = &record.folder;
    ensure!(
        folder.is_dir(),
        "beatmapset folder is missing: {}",
        folder.display()
    );
    let mut restored = 0;

    for name in &record.replaced {
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        let source = folder_backup_dir.join(name);
        let destination = folder.join(name);
        if !source.is_file() {
            continue;
        }
        fs::copy(&source, &destination).with_context(|| {
            format!(
                "restoring {} to {}",
                source.display(),
                destination.display()
            )
        })?;
        restored += 1;
    }

    for name in &record.created {
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        let path = folder.join(name);
        if path.is_file() {
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            restored += 1;
        }
    }
    Ok(restored)
}

/// The imported image, kept in its useful forms: raw bytes (lossless
/// passthrough when a target file has the same format) and the decoded
/// image (re-encoding into other target formats).
struct BackgroundPayload {
    source_ext: String,
    source_bytes: Arc<Vec<u8>>,
    decoded: image::DynamicImage,
}

impl BackgroundPayload {
    /// Bytes to write into a background file with the given extension:
    /// the raw import when the formats match, else one re-encode per
    /// distinct target extension. `None` marks an extension this build
    /// cannot encode (webp) — the file is skipped untouched.
    fn encoding_for(
        &self,
        ext: &str,
        cache: &mut HashMap<String, Arc<Vec<u8>>>,
    ) -> Option<Arc<Vec<u8>>> {
        if ext == self.source_ext {
            return Some(self.source_bytes.clone());
        }
        if let Some(bytes) = cache.get(ext) {
            return Some(bytes.clone());
        }
        let fitted = if self.decoded.width() > IMAGE_MAX_WIDTH {
            self.decoded.thumbnail(IMAGE_MAX_WIDTH, u32::MAX)
        } else {
            self.decoded.clone()
        };
        let encoded: Vec<u8> = match ext {
            "jpg" => {
                let rgb = fitted.to_rgb8();
                let mut out = Vec::new();
                image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY)
                    .write_image(
                        rgb.as_raw(),
                        rgb.width(),
                        rgb.height(),
                        image::ColorType::Rgb8,
                    )
                    .ok()?;
                out
            }
            "png" => {
                let mut out = Vec::new();
                fitted
                    .write_to(
                        &mut std::io::Cursor::new(&mut out),
                        image::ImageOutputFormat::Png,
                    )
                    .ok()?;
                out
            }
            "bmp" => {
                let mut out = Vec::new();
                fitted
                    .to_rgb8()
                    .write_to(
                        &mut std::io::Cursor::new(&mut out),
                        image::ImageOutputFormat::Bmp,
                    )
                    .ok()?;
                out
            }
            _ => return None,
        };
        let bytes = Arc::new(encoded);
        cache.insert(ext.to_owned(), bytes.clone());
        Some(bytes)
    }
}

fn normalize_ext(ext: &str) -> String {
    let lower = ext.to_ascii_lowercase();
    if lower == JPEG_ALIAS {
        "jpg".to_owned()
    } else {
        lower
    }
}

struct FolderApplyReport {
    /// Background files whose content was replaced (original backed up).
    replaced: Vec<String>,
    /// Background files created for charts referencing missing files.
    created: Vec<String>,
    /// Background files skipped because the cache says they already contain
    /// this exact image, untouched since.
    cached: usize,
    /// Referenced backgrounds left untouched (unsupported format).
    skipped: usize,
}

/// Reads every chart in the folder to find the background filenames they
/// reference, then overwrites those image files with the imported image.
/// Charts are opened read-only and never written.
fn apply_to_folder(
    folder: &Path,
    payload: &BackgroundPayload,
    fingerprint: &str,
    cache: &mut ExtrasCache,
    encodings: &mut HashMap<String, Arc<Vec<u8>>>,
    cancel: &AtomicBool,
    folder_backup_dir: &Path,
) -> Result<FolderApplyReport> {
    ensure!(
        folder.is_dir(),
        "beatmapset folder is missing: {}",
        folder.display()
    );

    // Collect the referenced background names, deduplicated case-wise (the
    // game resolves them case-insensitively) and sorted for determinism.
    let mut seen: HashSet<String> = HashSet::new();
    let mut backgrounds: Vec<String> = Vec::new();
    for entry in fs::read_dir(folder).with_context(|| format!("reading {}", folder.display()))? {
        let entry = entry.with_context(|| format!("reading {}", folder.display()))?;
        let path = entry.path();
        if !is_chart_file(&path) {
            continue;
        }
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        for name in chart_background_names(&bytes) {
            if seen.insert(name.to_ascii_lowercase()) {
                backgrounds.push(name);
            }
        }
    }
    backgrounds.sort_by_key(|name| name.to_ascii_lowercase());

    let mut report = FolderApplyReport {
        replaced: Vec::new(),
        created: Vec::new(),
        cached: 0,
        skipped: 0,
    };
    for name in backgrounds {
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        let target = folder.join(&name);
        if cache.is_fresh(&target, fingerprint) {
            // The file already contains this exact image and has not been
            // touched since the job wrote it: nothing to do.
            report.cached += 1;
            continue;
        }
        let ext = Path::new(&name)
            .extension()
            .and_then(|ext| ext.to_str())
            .map(normalize_ext)
            .unwrap_or_default();
        let Some(bytes) = payload.encoding_for(&ext, encodings) else {
            report.skipped += 1;
            continue;
        };
        let existed = target.is_file();
        if existed {
            // Keep the original content for the rollback.
            fs::create_dir_all(folder_backup_dir)
                .with_context(|| format!("creating {}", folder_backup_dir.display()))?;
            fs::copy(&target, folder_backup_dir.join(&name))
                .with_context(|| format!("backing up {}", target.display()))?;
        }
        fs::write(&target, bytes.as_slice())
            .with_context(|| format!("writing {}", target.display()))?;
        cache.record(&target, fingerprint);
        if existed {
            report.replaced.push(name);
        } else {
            report.created.push(name);
        }
    }
    Ok(report)
}

/// Every background filename referenced by the chart's `[Events]` section,
/// in file order. Sprite and video lines are deliberately ignored: their
/// images are storyboard art, not backgrounds. UTF-16 charts cannot be
/// scanned byte-wise and yield no names (they stay untouched).
fn chart_background_names(bytes: &[u8]) -> Vec<String> {
    if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) {
        return Vec::new();
    }
    let bom_len = if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        3
    } else {
        0
    };
    let body = &bytes[bom_len..];
    let mut names = Vec::new();
    let mut in_events = false;
    for (start, end, _) in split_lines(body) {
        let line = &body[start..end];
        if let Some(section) = section_name(line) {
            in_events = section == "events";
        } else if in_events
            && !is_comment_line(line)
            && is_background_event(line)
            && let Some(name) = background_filename(line)
        {
            names.push(name);
        }
    }
    names
}

fn write_manifest(manifest: &ExtrasManifest, manifest_path: &Path) -> Result<()> {
    let json = serde_json::to_string_pretty(manifest).context("serializing the manifest")?;
    if let Some(parent) = manifest_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(manifest_path, json).with_context(|| format!("writing {}", manifest_path.display()))
}

/// Per-folder backup directory names must be unique within one job and
/// path-safe; the manifest records the name actually used.
fn backup_dir_name(folder: &Path, used: &mut HashSet<String>) -> String {
    let base = folder
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "set".to_owned());
    let base: String = if base.chars().count() > 80 {
        base.chars().take(80).collect()
    } else {
        base
    };
    let mut name = base.clone();
    let mut suffix = 2;
    while !used.insert(name.to_ascii_lowercase()) {
        name = format!("{base}-{suffix}");
        suffix += 1;
    }
    name
}

/// Splits into lines as `(content_start, content_end, line_end)` where the
/// content excludes the trailing `\r` and `line_end` is just past the `\n`
/// (or `bytes.len()` when the last line has no newline).
fn split_lines(bytes: &[u8]) -> Vec<(usize, usize, usize)> {
    let mut lines = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        while i < bytes.len() && bytes[i] != b'\n' {
            i += 1;
        }
        let content_end = if i > start && bytes[i - 1] == b'\r' {
            i - 1
        } else {
            i
        };
        let line_end = (i + 1).min(bytes.len());
        lines.push((start, content_end, line_end));
        i = line_end;
    }
    lines
}

fn section_name(line: &[u8]) -> Option<String> {
    let trimmed = trim_ascii(line);
    if trimmed.len() >= 2 && trimmed[0] == b'[' && trimmed[trimmed.len() - 1] == b']' {
        Some(String::from_utf8_lossy(&trimmed[1..trimmed.len() - 1]).to_ascii_lowercase())
    } else {
        None
    }
}

fn trim_ascii(line: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = line.len();
    while start < end && matches!(line[start], b' ' | b'\t' | b'\r') {
        start += 1;
    }
    while end > start && matches!(line[end - 1], b' ' | b'\t' | b'\r') {
        end -= 1;
    }
    &line[start..end]
}

/// Mirrors `local::is_background_event`: first comma field is `0` or
/// `background` (osu! writes both spellings).
fn is_background_event(line: &[u8]) -> bool {
    let first = line.split(|&byte| byte == b',').next().unwrap_or_default();
    let first = String::from_utf8_lossy(first).trim().to_ascii_lowercase();
    matches!(first.as_str(), "0" | "background")
}

fn is_comment_line(line: &[u8]) -> bool {
    trim_ascii(line).starts_with(b"//")
}

/// The background event's filename is the third comma field, quoted or not.
fn background_filename(line: &[u8]) -> Option<String> {
    let fields = csv_fields(line);
    fields
        .get(2)
        .cloned()
        .filter(|value| is_image_filename(value))
}

/// Comma-split that respects double quotes, mirroring `local`'s parser.
fn csv_fields(line: &[u8]) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current: Vec<u8> = Vec::new();
    let mut in_quotes = false;
    for &byte in line {
        match byte {
            b'"' => in_quotes = !in_quotes,
            b',' if !in_quotes => {
                fields.push(
                    String::from_utf8_lossy(&current)
                        .trim_matches('"')
                        .trim()
                        .to_owned(),
                );
                current.clear();
            }
            _ => current.push(byte),
        }
    }
    fields.push(
        String::from_utf8_lossy(&current)
            .trim_matches('"')
            .trim()
            .to_owned(),
    );
    fields
}

fn is_image_filename(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() || value == "0" || !is_plain_filename(value) {
        return false;
    }
    Path::new(value)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| SUPPORTED_EXTS.contains(&ext.to_ascii_lowercase().as_str()))
}

/// Event filenames must stay inside the set folder; anything carrying a path
/// separator is not a file this tool touches.
fn is_plain_filename(value: &str) -> bool {
    !value.contains('/')
        && !value.contains('\\')
        && !value.contains(':')
        && value != "."
        && value != ".."
}

fn is_chart_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| matches!(ext.to_ascii_lowercase().as_str(), "osu" | "osb"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    const SAMPLE_OSU: &[u8] = b"osu file format v14\r\n\
[General]\r\n\
AudioFilename: audio.mp3\r\n\
[EditorDetails]\r\n\
distanceSpacing: 1.2\r\n\
[Events]\r\n\
//Background and Video events\r\n\
0,0,\"bg.jpg\",0,0\r\n\
//Background colour transformations\r\n\
3,100,50,50\r\n\
[TimingPoints]\r\n\
1000,300,4,2,0,60,1,0\r\n";

    fn drain_extras(rx: &mpsc::Receiver<ExtrasEvent>) -> Vec<ExtrasEvent> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    fn drain_rollback(rx: &mpsc::Receiver<RollbackEvent>) -> Vec<RollbackEvent> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    #[test]
    fn second_run_with_same_image_is_fully_cached() {
        let root = unique_temp_dir("osu-extras-cache");
        let folder = root.join("set");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("map.osu"), SAMPLE_OSU).unwrap();
        fs::write(folder.join("bg.jpg"), b"old jpeg content").unwrap();

        let mut jpeg = Vec::new();
        encoded_image("jpg", &mut jpeg);

        let run = |job: &str, tx: mpsc::Sender<ExtrasEvent>| {
            run_background_jobs(
                vec![folder.clone()],
                Arc::new(jpeg.clone()),
                "jpg".to_owned(),
                "wall.jpg".to_owned(),
                Arc::new(AtomicBool::new(false)),
                tx,
                root.join("extras-backups").join(job),
                root.join("extras-backups").join(format!("{job}.json")),
                root.join("extras_cache.json"),
            )
        };

        let (tx, rx) = mpsc::channel();
        run("job1", tx);
        for event in drain_extras(&rx) {
            if let ExtrasEvent::FolderDone { files, cached, .. } = event {
                assert_eq!((files, cached), (1, 0));
            }
        }
        // First job replaced the file and recorded the rollback manifest.
        assert!(root.join("extras-backups").join("job1.json").is_file());

        let (tx, rx) = mpsc::channel();
        run("job2", tx);
        let mut saw_cached = None;
        for event in drain_extras(&rx) {
            match event {
                ExtrasEvent::FolderDone { files, cached, .. } => {
                    saw_cached = Some((files, cached));
                }
                ExtrasEvent::Failed { message } => panic!("apply failed: {message}"),
                _ => {}
            }
        }
        // Second run skipped the file entirely: nothing replaced, nothing
        // backed up, and therefore no manifest for an empty job.
        assert_eq!(saw_cached, Some((0, 1)));
        assert!(!root.join("extras-backups").join("job2.json").exists());
        assert!(!root.join("extras-backups").join("job2").exists());
        assert_eq!(fs::read(folder.join("bg.jpg")).unwrap(), jpeg);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn switching_images_reapplies_and_refills_the_cache() {
        let root = unique_temp_dir("osu-extras-cache-image");
        let folder = root.join("set");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("map.osu"), SAMPLE_OSU).unwrap();
        fs::write(folder.join("bg.jpg"), b"old jpeg content").unwrap();

        let mut first = Vec::new();
        encoded_image("jpg", &mut first);
        let mut second = Vec::new();
        image::DynamicImage::new_rgb8(3, 2)
            .write_to(
                &mut std::io::Cursor::new(&mut second),
                image::ImageOutputFormat::Jpeg(90),
            )
            .unwrap();

        let run = |image: Vec<u8>, tx: mpsc::Sender<ExtrasEvent>| {
            run_background_jobs(
                vec![folder.clone()],
                Arc::new(image),
                "jpg".to_owned(),
                String::new(),
                Arc::new(AtomicBool::new(false)),
                tx,
                root.join("backups").join("job"),
                root.join("backups").join("job.json"),
                root.join("extras_cache.json"),
            )
        };

        let (tx, rx) = mpsc::channel();
        run(first, tx);
        drain_extras(&rx);
        // A different image must not be skipped, even though the file was
        // written by the previous job.
        let (tx, rx) = mpsc::channel();
        run(second.clone(), tx);
        for event in drain_extras(&rx) {
            if let ExtrasEvent::FolderDone { files, cached, .. } = event {
                assert_eq!((files, cached), (1, 0));
            }
        }
        assert_eq!(fs::read(folder.join("bg.jpg")).unwrap(), second);
        // And a third run of the newest image is cached again.
        let (tx, rx) = mpsc::channel();
        run(second, tx);
        for event in drain_extras(&rx) {
            if let ExtrasEvent::FolderDone { files, cached, .. } = event {
                assert_eq!((files, cached), (0, 1));
            }
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn outside_touches_and_rollback_invalidate_the_cache() {
        let root = unique_temp_dir("osu-extras-cache-inval");
        let folder = root.join("set");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("map.osu"), SAMPLE_OSU).unwrap();
        fs::write(folder.join("bg.jpg"), b"old jpeg content").unwrap();

        let mut jpeg = Vec::new();
        encoded_image("jpg", &mut jpeg);
        let paths = |job: &str| {
            (
                root.join("extras-backups").join(job),
                root.join("extras-backups").join(format!("{job}.json")),
            )
        };

        // First apply, then roll it back. The restored file has different
        // content (and a fresh mtime), so the cache entry must not count.
        let (job, manifest) = paths("job1");
        let (tx, rx) = mpsc::channel();
        run_background_jobs(
            vec![folder.clone()],
            Arc::new(jpeg.clone()),
            "jpg".to_owned(),
            String::new(),
            Arc::new(AtomicBool::new(false)),
            tx,
            job,
            manifest.clone(),
            root.join("extras_cache.json"),
        );
        drain_extras(&rx);
        let (tx, rx) = mpsc::channel();
        run_rollback_job(manifest, Arc::new(AtomicBool::new(false)), tx);
        drain_rollback(&rx);
        assert_eq!(
            fs::read(folder.join("bg.jpg")).unwrap(),
            b"old jpeg content"
        );

        // Re-applying after the rollback has real work to do again.
        let (job, manifest) = paths("job2");
        let (tx, rx) = mpsc::channel();
        run_background_jobs(
            vec![folder.clone()],
            Arc::new(jpeg.clone()),
            "jpg".to_owned(),
            String::new(),
            Arc::new(AtomicBool::new(false)),
            tx,
            job,
            manifest,
            root.join("extras_cache.json"),
        );
        for event in drain_extras(&rx) {
            if let ExtrasEvent::FolderDone { files, cached, .. } = event {
                assert_eq!((files, cached), (1, 0));
            }
        }

        // An outside edit (content changed under the same name) also
        // invalidates: the next apply rewrites the file.
        fs::write(folder.join("bg.jpg"), b"touched by hand").unwrap();
        let (job, manifest) = paths("job3");
        let (tx, rx) = mpsc::channel();
        run_background_jobs(
            vec![folder.clone()],
            Arc::new(jpeg.clone()),
            "jpg".to_owned(),
            String::new(),
            Arc::new(AtomicBool::new(false)),
            tx,
            job,
            manifest,
            root.join("extras_cache.json"),
        );
        for event in drain_extras(&rx) {
            if let ExtrasEvent::FolderDone { files, cached, .. } = event {
                assert_eq!((files, cached), (1, 0));
            }
        }
        assert_eq!(fs::read(folder.join("bg.jpg")).unwrap().len(), jpeg.len());
        let _ = fs::remove_dir_all(&root);
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

    fn payload(source_ext: &str, source_bytes: &[u8]) -> BackgroundPayload {
        BackgroundPayload {
            source_ext: source_ext.to_owned(),
            source_bytes: Arc::new(source_bytes.to_vec()),
            decoded: image::load_from_memory(source_bytes)
                .unwrap_or_else(|_| image::DynamicImage::new_rgb8(1, 1)),
        }
    }

    fn encoded_image(ext: &str, bytes: &mut Vec<u8>) {
        image::DynamicImage::new_rgb8(2, 2)
            .write_to(
                &mut std::io::Cursor::new(bytes),
                match ext {
                    "jpg" => image::ImageOutputFormat::Jpeg(90),
                    "png" => image::ImageOutputFormat::Png,
                    _ => image::ImageOutputFormat::Bmp,
                },
            )
            .unwrap();
    }

    #[test]
    fn chart_background_names_finds_only_background_lines() {
        let osu = b"osu file format v14\r\n[Events]\r\n0,0,\"bg.jpg\",0,0\r\nSprite,Foreground,Centre,\"sprite.png\",320,240\r\nVideo,0,\"video.mp4\"\r\n";
        assert_eq!(chart_background_names(osu), vec!["bg.jpg".to_owned()]);
        // Outside [Events] nothing counts, even a background-looking line.
        let osu = b"osu file format v14\r\n0,0,\"notabg.jpg\",0,0\r\n[Events]\r\n";
        assert!(chart_background_names(osu).is_empty());
        // UTF-8 BOM survives the scan.
        let mut bom: Vec<u8> = vec![0xEF, 0xBB, 0xBF];
        bom.extend_from_slice(b"osu file format v14\r\n[Events]\r\n0,0,\"bg.png\",0,0\r\n");
        assert_eq!(chart_background_names(&bom), vec!["bg.png".to_owned()]);
    }

    #[test]
    fn utf16_charts_yield_no_names() {
        let mut utf16: Vec<u8> = vec![0xFF, 0xFE];
        for unit in "[Events]\r\n0,0,\"bg.jpg\",0,0\r\n".encode_utf16() {
            utf16.extend_from_slice(&unit.to_le_bytes());
        }
        assert!(chart_background_names(&utf16).is_empty());
    }

    #[test]
    fn unquoted_filenames_and_offsets_are_parsed() {
        let osu = b"mo file format v9\r\n[Events]\r\n0,0,bg.JPG,4,-2\r\n";
        assert_eq!(chart_background_names(osu), vec!["bg.JPG".to_owned()]);
    }

    #[test]
    fn encoding_passthrough_and_reencodes() {
        let mut jpeg = Vec::new();
        encoded_image("jpg", &mut jpeg);
        let payload = payload("jpg", &jpeg);

        // Same format: the raw bytes pass through untouched.
        let direct = payload.encoding_for("jpg", &mut HashMap::new()).unwrap();
        assert_eq!(direct.as_slice(), jpeg.as_slice());
        // jpeg is folded into jpg by the callers (normalize_ext).
        assert_eq!(normalize_ext("jpeg"), "jpg");
        assert_eq!(normalize_ext("JPG"), "jpg");

        // Different formats re-encode with the right magic bytes.
        let png = payload.encoding_for("png", &mut HashMap::new()).unwrap();
        assert!(png.starts_with(&[0x89, b'P', b'N', b'G']));
        let bmp = payload.encoding_for("bmp", &mut HashMap::new()).unwrap();
        assert!(bmp.starts_with(b"BM"));

        // webp cannot be encoded by this build: skip the file untouched.
        assert!(payload.encoding_for("webp", &mut HashMap::new()).is_none());
    }

    #[test]
    fn apply_replaces_image_content_and_never_touches_charts() {
        let root = unique_temp_dir("osu-extras-apply");
        let folder = root.join("12345 Artist - Title");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("map.osu"), SAMPLE_OSU).unwrap();
        fs::write(folder.join("bg.jpg"), b"old jpeg content").unwrap();

        let mut jpeg = Vec::new();
        encoded_image("jpg", &mut jpeg);

        let job_backup_dir = root.join("extras-backups").join("job");
        let manifest_path = root.join("extras-backups").join("job.json");
        let (tx, rx) = mpsc::channel();
        run_background_jobs(
            vec![folder.clone()],
            Arc::new(jpeg.clone()),
            "jpg".to_owned(),
            "wall.jpg".to_owned(),
            Arc::new(AtomicBool::new(false)),
            tx,
            job_backup_dir.clone(),
            manifest_path.clone(),
            root.join("extras_cache.json"),
        );
        let events = drain_extras(&rx);
        assert!(matches!(
            events.first(),
            Some(ExtrasEvent::Started { folders: 1 })
        ));
        assert!(matches!(
            events.last(),
            Some(ExtrasEvent::Finished { files: 1, .. })
        ));

        // The chart is byte-identical; the image content changed.
        assert_eq!(fs::read(folder.join("map.osu")).unwrap(), SAMPLE_OSU);
        assert_eq!(fs::read(folder.join("bg.jpg")).unwrap(), jpeg);
        // The original image content is backed up and the manifest written.
        assert_eq!(
            fs::read(job_backup_dir.join("12345 Artist - Title").join("bg.jpg")).unwrap(),
            b"old jpeg content"
        );
        assert!(manifest_path.is_file());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn apply_creates_missing_backgrounds_and_skips_webp() {
        let root = unique_temp_dir("osu-extras-create");
        let folder = root.join("set");
        fs::create_dir_all(&folder).unwrap();
        fs::write(
            folder.join("missing.osu"),
            b"osu file format v14\r\n[Events]\r\n0,0,\"gone.png\",0,0\r\n",
        )
        .unwrap();
        fs::write(
            folder.join("webp.osu"),
            b"osu file format v14\r\n[Events]\r\n0,0,\"bg.webp\",0,0\r\n",
        )
        .unwrap();

        let mut jpeg = Vec::new();
        encoded_image("jpg", &mut jpeg);

        let job_backup_dir = root.join("backups").join("job");
        let manifest_path = root.join("backups").join("job.json");
        let (tx, rx) = mpsc::channel();
        run_background_jobs(
            vec![folder.clone()],
            Arc::new(jpeg),
            "jpg".to_owned(),
            String::new(),
            Arc::new(AtomicBool::new(false)),
            tx,
            job_backup_dir,
            manifest_path.clone(),
            root.join("extras_cache.json"),
        );
        for event in drain_extras(&rx) {
            match event {
                ExtrasEvent::FolderDone { files, skipped, .. } => {
                    assert_eq!((files, skipped), (1, 1));
                }
                ExtrasEvent::Failed { message } => panic!("apply failed: {message}"),
                _ => {}
            }
        }
        // The missing background was created as a real PNG (re-encoded from
        // the JPEG import); the webp file was never created.
        assert!(
            fs::read(folder.join("gone.png"))
                .unwrap()
                .starts_with(&[0x89, b'P'])
        );
        assert!(!folder.join("bg.webp").exists());
        // No chart was written.
        assert_eq!(
            fs::read(folder.join("missing.osu")).unwrap(),
            b"osu file format v14\r\n[Events]\r\n0,0,\"gone.png\",0,0\r\n"
        );
        assert_eq!(
            fs::read(folder.join("webp.osu")).unwrap(),
            b"osu file format v14\r\n[Events]\r\n0,0,\"bg.webp\",0,0\r\n"
        );
        // Only the created file needs a rollback record, not the webp.
        let manifest: ExtrasManifest =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        assert!(manifest.folders[0].replaced.is_empty());
        assert_eq!(manifest.folders[0].created, vec!["gone.png".to_owned()]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn apply_then_rollback_restores_original_content_exactly() {
        let root = unique_temp_dir("osu-extras-rollback");
        let folder = root.join("set");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("map.osu"), SAMPLE_OSU).unwrap();
        fs::write(folder.join("bg.jpg"), b"old jpeg content").unwrap();

        let mut jpeg = Vec::new();
        encoded_image("jpg", &mut jpeg);

        let backups = root.join("extras-backups");
        let job_backup_dir = backups.join("job1");
        let manifest_path = backups.join("job1.json");
        let (tx, rx) = mpsc::channel();
        run_background_jobs(
            vec![folder.clone()],
            Arc::new(jpeg),
            "jpg".to_owned(),
            "wall.jpg".to_owned(),
            Arc::new(AtomicBool::new(false)),
            tx,
            job_backup_dir.clone(),
            manifest_path.clone(),
            root.join("extras_cache.json"),
        );
        drain_extras(&rx);

        let (tx, rx) = mpsc::channel();
        run_rollback_job(manifest_path.clone(), Arc::new(AtomicBool::new(false)), tx);
        let mut restored_total = None;
        for event in drain_rollback(&rx) {
            match event {
                RollbackEvent::FolderDone { restored, .. } => restored_total = Some(restored),
                RollbackEvent::Failed { message } => panic!("rollback failed: {message}"),
                _ => {}
            }
        }
        assert_eq!(restored_total, Some(1));
        // The original image content is back, byte-identical, and the chart
        // never changed.
        assert_eq!(
            fs::read(folder.join("bg.jpg")).unwrap(),
            b"old jpeg content"
        );
        assert_eq!(fs::read(folder.join("map.osu")).unwrap(), SAMPLE_OSU);
        // A clean rollback removes its manifest and backup files.
        assert!(!manifest_path.exists());
        assert!(!job_backup_dir.exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_removes_files_the_job_created() {
        let root = unique_temp_dir("osu-extras-created");
        let folder = root.join("set");
        fs::create_dir_all(&folder).unwrap();
        fs::write(
            folder.join("map.osu"),
            b"osu file format v14\r\n[Events]\r\n0,0,\"gone.png\",0,0\r\n",
        )
        .unwrap();

        let mut png = Vec::new();
        encoded_image("png", &mut png);

        let job_backup_dir = root.join("backups").join("job");
        let manifest_path = root.join("backups").join("job.json");
        let (tx, rx) = mpsc::channel();
        run_background_jobs(
            vec![folder.clone()],
            Arc::new(png),
            "png".to_owned(),
            String::new(),
            Arc::new(AtomicBool::new(false)),
            tx,
            job_backup_dir.clone(),
            manifest_path.clone(),
            root.join("extras_cache.json"),
        );
        drain_extras(&rx);
        assert!(folder.join("gone.png").is_file());

        let (tx, rx) = mpsc::channel();
        run_rollback_job(manifest_path, Arc::new(AtomicBool::new(false)), tx);
        for event in drain_rollback(&rx) {
            if let RollbackEvent::Failed { message } = event {
                panic!("rollback failed: {message}");
            }
        }
        // The created file is gone again; the chart is untouched throughout.
        assert!(!folder.join("gone.png").exists());
        assert_eq!(
            fs::read(folder.join("map.osu")).unwrap(),
            b"osu file format v14\r\n[Events]\r\n0,0,\"gone.png\",0,0\r\n"
        );
        assert!(!job_backup_dir.exists());
        let _ = fs::remove_dir_all(&root);
    }
}
