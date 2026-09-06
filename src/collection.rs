use crate::local::LocalBeatmap;
use anyhow::{Context, Result};
use std::{fs, io::Write, path::Path};

const DEFAULT_COLLECTION_DB_VERSION: i32 = 20250107;

#[derive(Debug, Clone)]
pub struct CollectionEntry {
    pub name: String,
    pub hashes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CollectionDb {
    pub version: i32,
    pub collections: Vec<CollectionEntry>,
}

impl CollectionDb {
    pub fn empty() -> Self {
        Self {
            version: DEFAULT_COLLECTION_DB_VERSION,
            collections: Vec::new(),
        }
    }
}

pub fn load_collection_db(path: &Path) -> Result<CollectionDb> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    parse_collection_db(&bytes).with_context(|| format!("parsing {}", path.display()))
}

pub fn create_collection(path: &Path, collection_name: &str) -> Result<()> {
    let mut db = if path.exists() {
        load_collection_db(path)?
    } else {
        CollectionDb::empty()
    };

    upsert_collection_hashes(&mut db, collection_name, Vec::new());
    write_db(path, &db)
}

pub fn add_to_collection(
    path: &Path,
    collection_name: &str,
    hashes: &[String],
) -> Result<()> {
    let mut db = if path.exists() {
        load_collection_db(path)?
    } else {
        CollectionDb::empty()
    };

    let mut existing = db
        .collections
        .iter()
        .find(|c| c.name == collection_name)
        .map(|c| c.hashes.clone())
        .unwrap_or_default();

    for hash in hashes {
        if !existing.contains(hash) {
            existing.push(hash.clone());
        }
    }
    upsert_collection_hashes(&mut db, collection_name, existing);
    write_db(path, &db)
}

pub fn upsert_collection_hashes(db: &mut CollectionDb, collection_name: &str, hashes: Vec<String>) {
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

pub fn delete_collection(db: &mut CollectionDb, collection_name: &str) -> bool {
    let original_len = db.collections.len();
    db.collections
        .retain(|collection| collection.name != collection_name);
    db.collections.len() != original_len
}

pub fn write_db(path: &Path, db: &CollectionDb) -> Result<()> {
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

    #[test]
    fn saves_renames_and_deletes_collections() {
        let path = std::env::temp_dir().join(format!(
            "osu-map-manager-collection-test-{}.db",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(path.with_extension("db.bak"));

        let mut db = CollectionDb::empty();
        upsert_collection_hashes(&mut db, "first", vec!["aaa".to_owned(), "bbb".to_owned()]);
        upsert_collection_hashes(&mut db, "second", vec!["ccc".to_owned()]);
        write_db(&path, &db).unwrap();

        let mut loaded = load_collection_db(&path).unwrap();
        assert_eq!(loaded.collections.len(), 2);
        assert_eq!(loaded.collections[0].name, "first");
        assert_eq!(loaded.collections[0].hashes, ["aaa", "bbb"]);

        assert!(delete_collection(&mut loaded, "first"));
        upsert_collection_hashes(&mut loaded, "renamed", vec!["bbb".to_owned()]);
        write_db(&path, &loaded).unwrap();

        let reloaded = load_collection_db(&path).unwrap();
        assert_eq!(
            reloaded
                .collections
                .iter()
                .map(|collection| collection.name.as_str())
                .collect::<Vec<_>>(),
            ["second", "renamed"]
        );
        assert!(path.with_extension("db.bak").exists());

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(path.with_extension("db.bak"));
    }
}
