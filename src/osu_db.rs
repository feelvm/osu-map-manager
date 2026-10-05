use anyhow::{Context, Result};
use std::{
    collections::BTreeMap,
    fs,
    io::{Cursor, Read},
    path::Path,
};

/// osu!.db format versions with field-level changes this parser branches on
/// (dates follow osu! stable build numbers).
const CHANGE_20140609: i32 = 20140609;
const CHANGE_20191106: i32 = 20191106;
/// Star-rating pairs shrank from tagged Int-Double to tagged Int-Float.
const CHANGE_20250107: i32 = 20250107;

#[derive(Debug, Clone, Default)]
pub struct DbBeatmapMeta {
    pub md5: String,
    pub osu_filename: String,
    pub standard_stars: Option<f32>,
    /// osu!'s locally cached rank status for this difficulty. See
    /// [`ranked_status_name`] for the meaning of each value; `0` (unknown)
    /// is the struct default when the entry came from an older cache.
    pub ranked_status: u8,
}

/// osu!.db rank-status bytes that mark a set osu! will not publish updates
/// for while it holds that status.
pub const DB_STATUS_RANKED: u8 = 4;
pub const DB_STATUS_APPROVED: u8 = 5;
pub const DB_STATUS_QUALIFIED: u8 = 6;
pub const DB_STATUS_LOVED: u8 = 7;

/// Human-readable name for an osu!.db rank-status byte.
pub fn ranked_status_name(status: u8) -> &'static str {
    match status {
        1 => "unsubmitted",
        2 => "pending",
        DB_STATUS_RANKED => "ranked",
        DB_STATUS_APPROVED => "approved",
        DB_STATUS_QUALIFIED => "qualified",
        DB_STATUS_LOVED => "loved",
        _ => "unknown",
    }
}

#[derive(Debug, Clone, Default)]
pub struct OsuDbIndex {
    by_md5: BTreeMap<String, DbBeatmapMeta>,
    by_filename: BTreeMap<String, DbBeatmapMeta>,
}

impl OsuDbIndex {
    pub fn load(osu_root: &Path) -> Result<Self> {
        let path = osu_root.join("osu!.db");
        let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        parse_osu_db(&bytes).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn get(&self, md5: &str, osu_filename: &str) -> Option<&DbBeatmapMeta> {
        self.by_md5
            .get(md5)
            .or_else(|| self.by_filename.get(&osu_filename.to_ascii_lowercase()))
    }
}

fn parse_osu_db(bytes: &[u8]) -> Result<OsuDbIndex> {
    let mut reader = DbReader::new(bytes);
    let version = reader.i32()?;
    reader.i32()?;
    reader.bool()?;
    reader.datetime_ticks()?;
    reader.string()?;
    let beatmap_count = reader.i32()?.max(0) as usize;

    let mut index = OsuDbIndex::default();
    for _ in 0..beatmap_count {
        if let Some(meta) = read_beatmap(&mut reader, version)? {
            if !meta.md5.is_empty() {
                index.by_md5.insert(meta.md5.clone(), meta.clone());
            }
            if !meta.osu_filename.is_empty() {
                index
                    .by_filename
                    .insert(meta.osu_filename.to_ascii_lowercase(), meta);
            }
        }
    }

    Ok(index)
}

/// Reads one per-difficulty beatmap entry, following the field order
/// documented in the osu! wiki's "Legacy database file structure" page:
/// a leading entry-size Int exists only before version 20191106, seven
/// strings precede the md5 hash, and AR/CS/HP/OD are Singles (not Bytes)
/// since version 20140609.
fn read_beatmap(reader: &mut DbReader<'_>, version: i32) -> Result<Option<DbBeatmapMeta>> {
    if version < CHANGE_20191106 {
        reader.i32()?; // size in bytes of the beatmap entry
    }
    // Artist, artist unicode, title, title unicode, creator, difficulty,
    // audio file name — none of them consumed.
    for _ in 0..7 {
        reader.string()?;
    }
    let md5 = reader.string()?;
    let osu_filename = reader.string()?;
    let ranked_status = reader.u8()?;
    reader.i16()?; // hitcircles
    reader.i16()?; // sliders
    reader.i16()?; // spinners
    reader.i64()?; // last modification time, Windows ticks
    if version < CHANGE_20140609 {
        for _ in 0..4 {
            reader.u8()?; // AR/CS/HP/OD as bytes
        }
    } else {
        for _ in 0..4 {
            reader.single()?; // AR/CS/HP/OD as Singles
        }
    }
    reader.double()?; // slider velocity

    let standard_stars = if version >= CHANGE_20140609 {
        read_star_rating_pairs(reader, version)?
    } else {
        None
    };
    if version >= CHANGE_20140609 {
        read_star_rating_pairs(reader, version)?; // taiko
        read_star_rating_pairs(reader, version)?; // catch
        read_star_rating_pairs(reader, version)?; // mania
    }

    reader.i32()?; // drain time (seconds)
    reader.i32()?; // total time (milliseconds)
    reader.i32()?; // audio preview start (milliseconds)
    let timing_points = reader.i32()?.max(0) as usize;
    for _ in 0..timing_points {
        reader.double()?; // BPM (negative when inherited)
        reader.double()?; // offset
        reader.bool()?; // uninherited (timing change)
    }
    reader.i32()?; // difficulty id
    reader.i32()?; // beatmap id
    reader.i32()?; // thread id
    reader.u8()?; // grade for osu!
    reader.u8()?; // grade for taiko
    reader.u8()?; // grade for catch
    reader.u8()?; // grade for mania
    reader.i16()?; // local beatmap offset
    reader.single()?; // stack leniency
    reader.u8()?; // gameplay mode
    reader.string()?; // song source
    reader.string()?; // song tags
    reader.i16()?; // online offset
    reader.string()?; // title font
    reader.bool()?; // is beatmap unplayed
    reader.i64()?; // last time played
    reader.bool()?; // is osz2
    reader.string()?; // folder name relative to Songs
    reader.i64()?; // last time checked against the repository
    reader.bool()?; // ignore beatmap sound
    reader.bool()?; // ignore beatmap skin
    reader.bool()?; // disable storyboard
    reader.bool()?; // disable video
    reader.bool()?; // visual override
    if version < 20140609 {
        reader.i16()?; // unknown
    }
    reader.i32()?; // last modification time
    reader.u8()?; // mania scroll speed

    Ok(Some(DbBeatmapMeta {
        md5,
        osu_filename,
        standard_stars,
        ranked_status,
    }))
}

/// One star-rating table: an Int pair count followed by (mods, stars)
/// pairs, each written with osu!'s serialization type tags — 0x08 marks an
/// Int32 (the mods bitmask); the star value is a tagged Double (0x0d) before
/// the 20250107 breaking change and a tagged Single (0x0c) since. Returns
/// the no-mod value when present.
fn read_star_rating_pairs(reader: &mut DbReader<'_>, version: i32) -> Result<Option<f32>> {
    let count = reader.i32()?.max(0) as usize;
    let mut no_mod = None;
    for _ in 0..count {
        let int_tag = reader.u8()?;
        if int_tag != 0x08 {
            anyhow::bail!("invalid star rating int tag {int_tag:#04x}");
        }
        let mods = reader.i32()?;
        let stars_tag = reader.u8()?;
        let stars = match (version < CHANGE_20250107, stars_tag) {
            (true, 0x0d) => reader.double()? as f32,
            (false, 0x0c) => reader.single()?,
            (_, tag) => anyhow::bail!("invalid star rating value tag {tag:#04x}"),
        };
        if mods == 0 {
            no_mod = Some(stars);
        }
    }
    Ok(no_mod)
}

/// Sanity cap for a single osu!.db string: names and paths are bytes to
/// kilobytes in practice; anything larger is corruption, not content.
const MAX_OSU_DB_STRING_LEN: u64 = 1_000_000;

struct DbReader<'a> {
    cursor: Cursor<&'a [u8]>,
}

impl<'a> DbReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            cursor: Cursor::new(bytes),
        }
    }

    fn u8(&mut self) -> Result<u8> {
        let mut buf = [0; 1];
        self.cursor.read_exact(&mut buf)?;
        Ok(buf[0])
    }

    fn bool(&mut self) -> Result<bool> {
        Ok(self.u8()? != 0)
    }

    fn i16(&mut self) -> Result<i16> {
        let mut buf = [0; 2];
        self.cursor.read_exact(&mut buf)?;
        Ok(i16::from_le_bytes(buf))
    }

    fn i32(&mut self) -> Result<i32> {
        let mut buf = [0; 4];
        self.cursor.read_exact(&mut buf)?;
        Ok(i32::from_le_bytes(buf))
    }

    fn i64(&mut self) -> Result<i64> {
        let mut buf = [0; 8];
        self.cursor.read_exact(&mut buf)?;
        Ok(i64::from_le_bytes(buf))
    }

    fn double(&mut self) -> Result<f64> {
        let mut buf = [0; 8];
        self.cursor.read_exact(&mut buf)?;
        Ok(f64::from_le_bytes(buf))
    }

    fn single(&mut self) -> Result<f32> {
        let mut buf = [0; 4];
        self.cursor.read_exact(&mut buf)?;
        Ok(f32::from_le_bytes(buf))
    }

    fn datetime_ticks(&mut self) -> Result<i64> {
        self.i64()
    }

    fn string(&mut self) -> Result<String> {
        let marker = self.u8()?;
        if marker == 0 {
            return Ok(String::new());
        }
        if marker != 0x0b {
            anyhow::bail!("invalid osu!.db string marker {marker}");
        }

        let len = self.uleb128()?;
        // A corrupt length prefix must not cause a gigantic allocation.
        if len > MAX_OSU_DB_STRING_LEN {
            anyhow::bail!("osu!.db string length {len} exceeds sanity limit");
        }
        let mut buf = vec![0; len as usize];
        self.cursor.read_exact(&mut buf)?;
        // Lossy on purpose: real libraries contain non-UTF-8 artist/title
        // bytes (legacy encodings), and dropping the whole osu!.db over one
        // bad name would be worse than a replacement character.
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    fn uleb128(&mut self) -> Result<u64> {
        let mut value = 0_u64;
        let mut shift = 0;
        loop {
            let byte = self.u8()?;
            value |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
            if shift >= 64 {
                anyhow::bail!("uleb128 value is too large");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlong_uleb128_is_rejected() {
        // 11 continuation bytes: no u64 can be that long.
        let bytes = vec![0x80; 11];
        assert!(DbReader::new(&bytes).uleb128().is_err());
        // Largest valid encoding still decodes.
        let max = vec![0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01];
        assert_eq!(DbReader::new(&max).uleb128().unwrap(), u64::MAX);
    }

    #[test]
    fn absurd_string_length_is_rejected_without_allocating() {
        // 0x0b marker + uleb128(u64::MAX): must bail before any big read.
        let bytes = vec![
            0x0b, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01,
        ];
        assert!(DbReader::new(&bytes).string().is_err());
        // Empty and tiny strings still work.
        assert_eq!(DbReader::new(&[0]).string().unwrap(), "");
    }

    #[test]
    fn ranked_status_is_parsed_from_modern_entries() {
        for status in [
            0_u8,
            2,
            DB_STATUS_RANKED,
            DB_STATUS_APPROVED,
            DB_STATUS_QUALIFIED,
            7,
        ] {
            // Current osu! stable writes Float star ratings (20250107+).
            let bytes = fixture_db(status, 20260101, None);
            let index = parse_osu_db(&bytes).unwrap();
            let meta = index.get("md5hash", "map.osu").unwrap();
            assert_eq!(meta.ranked_status, status);
            assert_eq!(meta.md5, "md5hash");
            assert_eq!(meta.osu_filename, "map.osu");
            assert_eq!(meta.standard_stars, Some(5.67));
        }
    }

    #[test]
    fn star_ratings_are_doubles_before_20250107() {
        let bytes = fixture_db(DB_STATUS_LOVED, 20250106, None);
        let index = parse_osu_db(&bytes).unwrap();
        let meta = index.get("md5hash", "map.osu").unwrap();
        assert_eq!(meta.ranked_status, DB_STATUS_LOVED);
        assert_eq!(meta.standard_stars, Some(5.67));
    }

    #[test]
    fn legacy_entries_carry_a_leading_entry_size_int() {
        // version < 20191106: entry starts with its byte size, AR/CS/HP/OD
        // are still Singles and star tables are present.
        let bytes = fixture_db(DB_STATUS_RANKED, 20191105, Some(42));
        let index = parse_osu_db(&bytes).unwrap();
        let meta = index.get("md5hash", "map.osu").unwrap();
        assert_eq!(meta.ranked_status, DB_STATUS_RANKED);
        assert_eq!(meta.standard_stars, Some(5.67));
    }

    #[test]
    fn pre_20140609_entries_use_byte_difficulties_and_no_star_tables() {
        let bytes = fixture_db(DB_STATUS_QUALIFIED, 20140608, Some(1));
        let index = parse_osu_db(&bytes).unwrap();
        let meta = index.get("md5hash", "map.osu").unwrap();
        assert_eq!(meta.ranked_status, DB_STATUS_QUALIFIED);
        assert_eq!(meta.standard_stars, None);
    }

    /// Builds a minimal osu!.db with one entry in the field order documented
    /// by the osu! wiki's "Legacy database file structure" page. `entry_size`
    /// is only written for versions below 20191106.
    fn fixture_db(status: u8, version: i32, entry_size: Option<i32>) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend(version.to_le_bytes());
        b.extend(0_i32.to_le_bytes()); // folder count
        b.push(0); // account unlocked
        b.extend(0_i64.to_le_bytes()); // last import datetime
        b.push(0); // player name: empty string
        b.extend(1_i32.to_le_bytes()); // beatmap count
        // Beatmap entry:
        if let Some(entry_size) = entry_size {
            b.extend(entry_size.to_le_bytes());
        }
        for _ in 0..7 {
            push_db_string(&mut b, b"x"); // artist, artist unicode, title,
            // title unicode, creator, difficulty, audio file name
        }
        push_db_string(&mut b, b"md5hash");
        push_db_string(&mut b, b"map.osu");
        b.push(status); // ranked status
        b.extend([0_u8; 6]); // hitcircle/slider/spinner counts (i16 x3)
        b.extend([0; 8]); // last modification time (i64)
        if version < 20140609 {
            b.extend([0_u8; 4]); // AR/CS/HP/OD as bytes
        } else {
            b.extend([0; 16]); // AR/CS/HP/OD as Singles
        }
        b.extend([0; 8]); // slider velocity (f64)
        if version >= 20140609 {
            for stars in [5.67_f64, 4.0, 3.0, 2.0] {
                // one no-mod pair per table (standard/taiko/catch/mania),
                // written with osu!'s serialization type tags; the star
                // value is a Double before 20250107 and a Float since
                b.extend(1_i32.to_le_bytes());
                b.push(0x08);
                b.extend(0_i32.to_le_bytes());
                if version >= 20250107 {
                    b.push(0x0c);
                    b.extend((stars as f32).to_le_bytes());
                } else {
                    b.push(0x0d);
                    b.extend(stars.to_le_bytes());
                }
            }
        }
        b.extend(0_i32.to_le_bytes()); // drain time (s)
        b.extend(0_i32.to_le_bytes()); // total time (ms)
        b.extend(0_i32.to_le_bytes()); // audio preview start (ms)
        b.extend(1_i32.to_le_bytes()); // timing point count
        b.extend([0; 8]); // timing point BPM
        b.extend([0; 8]); // timing point offset
        b.push(1); // timing point uninherited
        b.extend(0_i32.to_le_bytes()); // difficulty id
        b.extend(0_i32.to_le_bytes()); // beatmap id
        b.extend(0_i32.to_le_bytes()); // thread id
        b.extend([0_u8; 4]); // grades for osu!/taiko/catch/mania
        b.extend([0_u8; 2]); // local offset (i16)
        b.extend([0; 4]); // stack leniency (Single)
        b.push(0); // gameplay mode
        b.push(0); // song source: empty string
        b.push(0); // song tags: empty string
        b.extend([0_u8; 2]); // online offset (i16)
        b.push(0); // title font: empty string
        b.push(1); // is unplayed
        b.extend(0_i64.to_le_bytes()); // last time played
        b.push(0); // is osz2
        push_db_string(&mut b, b"folder"); // folder name
        b.extend(0_i64.to_le_bytes()); // last time checked
        b.extend([0_u8; 5]); // ignore sound/skin, no storyboard/video, visual override
        if version < 20140609 {
            b.extend([0_u8; 2]); // unknown (i16)
        }
        b.extend(0_i32.to_le_bytes()); // last modification time
        b.push(0); // mania scroll speed
        b
    }

    fn push_db_string(b: &mut Vec<u8>, value: &[u8]) {
        b.push(0x0b);
        b.push(value.len() as u8); // single-byte uleb128 length
        b.extend_from_slice(value);
    }
}
