use crate::local::LocalBeatmap;
use anyhow::{Context, Result};
use std::{fs, io::Write, path::Path};

pub fn write_collection_db(
    path: &Path,
    collection_name: &str,
    maps: &[LocalBeatmap],
) -> Result<()> {
    let mut bytes = Vec::new();
    write_i32(&mut bytes, 20250107)?;
    write_i32(&mut bytes, 1)?;
    write_osu_string(&mut bytes, collection_name)?;
    write_i32(&mut bytes, maps.len() as i32)?;
    for map in maps {
        write_osu_string(&mut bytes, &map.md5)?;
    }

    if path.exists() {
        let backup = path.with_extension("db.bak");
        fs::copy(path, &backup).with_context(|| format!("backing up {}", path.display()))?;
    }
    fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))
}

pub fn write_manifest(path: &Path, maps: &[LocalBeatmap]) -> Result<()> {
    let mut file =
        fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    for map in maps {
        writeln!(
            file,
            "{}\t{}\t{}\t{}",
            map.md5,
            map.beatmapset_id
                .map_or_else(String::new, |id| id.to_string()),
            map.beatmap_id.map_or_else(String::new, |id| id.to_string()),
            map.label()
        )?;
    }
    Ok(())
}

fn write_i32(bytes: &mut Vec<u8>, value: i32) -> Result<()> {
    bytes.write_all(&value.to_le_bytes())?;
    Ok(())
}

fn write_osu_string(bytes: &mut Vec<u8>, value: &str) -> Result<()> {
    if value.is_empty() {
        bytes.push(0);
        return Ok(());
    }

    bytes.push(0x0b);
    write_uleb128(bytes, value.len() as u64);
    bytes.write_all(value.as_bytes())?;
    Ok(())
}

fn write_uleb128(bytes: &mut Vec<u8>, mut value: u64) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        bytes.push(byte);
        if value == 0 {
            break;
        }
    }
}
