use crate::local::LocalBeatmap;
use anyhow::{Context, Result};
use std::{fs, io::Write, path::Path};

const DEFAULT_COLLECTION_DB_VERSION: i32 = 20250107;

#[derive(Debug, Clone)]
struct CollectionEntry {
    name: String,
    hashes: Vec<String>,
}

#[derive(Debug, Clone)]
struct CollectionDb {
    version: i32,
    collections: Vec<CollectionEntry>,
}

pub fn write_collection_db(
    path: &Path,
    collection_name: &str,
    maps: &[LocalBeatmap],
) -> Result<()> {
    let mut db = if path.exists() {
        let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        parse_collection_db(&bytes).with_context(|| format!("parsing {}", path.display()))?
    } else {
        CollectionDb {
            version: DEFAULT_COLLECTION_DB_VERSION,
            collections: Vec::new(),
        }
    };

    let hashes = maps.iter().map(|map| map.md5.clone()).collect::<Vec<_>>();
    if let Some(collection) = db
        .collections
        .iter_mut()
        .find(|collection| collection.name == collection_name)
    {
        collection.hashes = hashes;
    } else {
        db.collections.push(CollectionEntry {
            name: collection_name.to_owned(),
            hashes,
        });
    }

    let mut bytes = Vec::new();
    write_i32(&mut bytes, db.version)?;
    write_i32(&mut bytes, db.collections.len() as i32)?;
    for collection in &db.collections {
        write_osu_string(&mut bytes, &collection.name)?;
        write_i32(&mut bytes, collection.hashes.len() as i32)?;
        for hash in &collection.hashes {
            write_osu_string(&mut bytes, hash)?;
        }
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

pub fn restore_collection_backup(path: &Path) -> Result<()> {
    let backup = path.with_extension("db.bak");
    if !backup.exists() {
        anyhow::bail!("backup does not exist: {}", backup.display());
    }

    if path.exists() {
        let before_restore = path.with_extension("db.before-restore");
        fs::copy(path, &before_restore).with_context(|| {
            format!(
                "saving current collection.db to {}",
                before_restore.display()
            )
        })?;
    }
    fs::copy(&backup, path)
        .with_context(|| format!("restoring {} to {}", backup.display(), path.display()))?;
    Ok(())
}

fn write_i32(bytes: &mut Vec<u8>, value: i32) -> Result<()> {
    bytes.write_all(&value.to_le_bytes())?;
    Ok(())
}

fn parse_collection_db(bytes: &[u8]) -> Result<CollectionDb> {
    let mut cursor = ByteCursor { bytes, offset: 0 };
    let version = cursor.read_i32()?;
    let collection_count = cursor.read_i32()?;
    if collection_count < 0 {
        anyhow::bail!("negative collection count");
    }

    let mut collections = Vec::with_capacity(collection_count as usize);
    for _ in 0..collection_count {
        let name = cursor.read_osu_string()?;
        let hash_count = cursor.read_i32()?;
        if hash_count < 0 {
            anyhow::bail!("negative beatmap count for collection {name}");
        }

        let mut hashes = Vec::with_capacity(hash_count as usize);
        for _ in 0..hash_count {
            hashes.push(cursor.read_osu_string()?);
        }
        collections.push(CollectionEntry { name, hashes });
    }

    Ok(CollectionDb {
        version,
        collections,
    })
}

struct ByteCursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl ByteCursor<'_> {
    fn read_i32(&mut self) -> Result<i32> {
        let bytes = self.read_exact(4)?;
        Ok(i32::from_le_bytes(bytes.try_into().expect("fixed length")))
    }

    fn read_osu_string(&mut self) -> Result<String> {
        let marker = self.read_u8()?;
        match marker {
            0 => Ok(String::new()),
            0x0b => {
                let len = self.read_uleb128()? as usize;
                let bytes = self.read_exact(len)?;
                String::from_utf8(bytes.to_vec()).context("collection string is not valid UTF-8")
            }
            value => anyhow::bail!("invalid osu string marker {value:#x}"),
        }
    }

    fn read_u8(&mut self) -> Result<u8> {
        Ok(self.read_exact(1)?[0])
    }

    fn read_exact(&mut self, len: usize) -> Result<&[u8]> {
        let end = self
            .offset
            .checked_add(len)
            .context("collection.db offset overflow")?;
        if end > self.bytes.len() {
            anyhow::bail!("unexpected end of collection.db");
        }
        let bytes = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(bytes)
    }

    fn read_uleb128(&mut self) -> Result<u64> {
        let mut value = 0_u64;
        let mut shift = 0;
        loop {
            let byte = self.read_u8()?;
            value |= u64::from(byte & 0x7f) << shift;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updates_named_collection_without_removing_others() {
        let db = CollectionDb {
            version: 20250107,
            collections: vec![
                CollectionEntry {
                    name: "keep".to_owned(),
                    hashes: vec!["aaa".to_owned()],
                },
                CollectionEntry {
                    name: "replace".to_owned(),
                    hashes: vec!["old".to_owned()],
                },
            ],
        };
        let mut bytes = Vec::new();
        write_i32(&mut bytes, db.version).unwrap();
        write_i32(&mut bytes, db.collections.len() as i32).unwrap();
        for collection in &db.collections {
            write_osu_string(&mut bytes, &collection.name).unwrap();
            write_i32(&mut bytes, collection.hashes.len() as i32).unwrap();
            for hash in &collection.hashes {
                write_osu_string(&mut bytes, hash).unwrap();
            }
        }

        let parsed = parse_collection_db(&bytes).unwrap();
        assert_eq!(parsed.collections.len(), 2);
        assert_eq!(parsed.collections[0].name, "keep");
        assert_eq!(parsed.collections[0].hashes, ["aaa"]);
        assert_eq!(parsed.collections[1].name, "replace");
        assert_eq!(parsed.collections[1].hashes, ["old"]);
    }
}
