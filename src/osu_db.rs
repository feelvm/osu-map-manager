use anyhow::{Context, Result};
use std::{
    collections::BTreeMap,
    fs,
    io::{Cursor, Read},
    path::Path,
};

#[derive(Debug, Clone, Default)]
pub struct DbBeatmapMeta {
    pub md5: String,
    pub osu_filename: String,
    pub standard_stars: Option<f32>,
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

fn read_beatmap(reader: &mut DbReader<'_>, version: i32) -> Result<Option<DbBeatmapMeta>> {
    reader.i32()?;
    reader.string()?;
    reader.string()?;
    reader.string()?;
    reader.string()?;
    reader.string()?;
    reader.string()?;
    let md5 = reader.string()?;
    let osu_filename = reader.string()?;
    reader.u8()?;
    reader.i16()?;
    reader.i16()?;
    reader.i16()?;
    reader.i64()?;
    reader.i32()?;
    reader.i32()?;
    reader.i32()?;
    reader.i32()?;
    reader.i32()?;
    reader.i32()?;
    reader.u8()?;
    reader.u8()?;
    reader.u8()?;
    reader.u8()?;
    reader.double()?;

    let standard_stars = if version < 20140609 {
        reader.double().ok().map(|value| value as f32)
    } else {
        read_star_rating_pairs(reader)?
    };

    if version >= 20140609 {
        let _taiko = read_star_rating_pairs(reader)?;
        let _catch = read_star_rating_pairs(reader)?;
        let _mania = read_star_rating_pairs(reader)?;
    }

    reader.i32()?;
    reader.i32()?;
    reader.i32()?;
    reader.i32()?;
    reader.i32()?;
    reader.i32()?;
    reader.i32()?;
    reader.i32()?;
    reader.i16()?;
    reader.double()?;
    if version >= 20140609 {
        reader.double()?;
    }
    reader.double()?;
    reader.double()?;
    reader.bool()?;
    reader.bool()?;
    reader.bool()?;
    reader.bool()?;
    reader.i32()?;
    reader.i32()?;
    reader.u8()?;
    reader.string()?;
    reader.string()?;
    reader.i16()?;
    reader.i64()?;
    reader.bool()?;

    Ok(Some(DbBeatmapMeta {
        md5,
        osu_filename,
        standard_stars,
    }))
}

fn read_star_rating_pairs(reader: &mut DbReader<'_>) -> Result<Option<f32>> {
    let count = reader.i32()?.max(0) as usize;
    let mut no_mod = None;
    for _ in 0..count {
        reader.u8()?;
        let mods = reader.i32()?;
        reader.u8()?;
        let stars = reader.double()? as f32;
        if mods == 0 {
            no_mod = Some(stars);
        }
    }
    Ok(no_mod)
}

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

        let len = self.uleb128()? as usize;
        let mut buf = vec![0; len];
        self.cursor.read_exact(&mut buf)?;
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
        }
    }
}
