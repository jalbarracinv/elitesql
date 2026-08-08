//! Atomic manifest for the disposable primary-index LSM runs.
//!
//! Canonical visibility still comes from `manifest` + segments + WAL. This
//! file only says which immutable paged runs together cover one exact segment
//! generation. A missing, stale, or damaged run manifest is rebuilt from the
//! canonical segments.

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::manifest::fsync_dir;

pub(crate) const PRIMARY_RUN_MANIFEST: &str = "primary.runs";
const PRIMARY_RUN_MANIFEST_TMP: &str = "primary.runs.tmp";
const MAGIC: &[u8; 8] = b"ESQLRUN1";
const DERIVED_MAGIC: &[u8; 8] = b"ESQLDRN1";
const FORMAT: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PrimaryRunMeta {
    pub file: String,
    pub level: u8,
    pub bytes: u64,
    pub generation: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PrimaryRunManifest {
    format: u32,
    pub generation: u64,
    pub runs: Vec<PrimaryRunMeta>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum DerivedRunKind {
    Secondary,
    Text,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DerivedRunMeta {
    pub file: String,
    pub level: u8,
    pub bytes: u64,
    pub generation: u64,
}

/// Run set for one disposable equality or full-text index. `aux` is reserved
/// for kind-specific exact aggregate state (BM25 uses document count and total
/// token length); equality indexes leave it at zero.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DerivedRunManifest {
    format: u32,
    pub kind: DerivedRunKind,
    pub table: String,
    pub column: String,
    pub generation: u64,
    pub runs: Vec<DerivedRunMeta>,
    pub aux: [u64; 2],
}

impl PrimaryRunManifest {
    pub(crate) fn new(generation: u64, runs: Vec<PrimaryRunMeta>) -> Self {
        Self {
            format: FORMAT,
            generation,
            runs,
        }
    }

    fn encode(&self) -> Vec<u8> {
        let body = serde_json::to_vec(self).expect("run manifest always serializes");
        let mut bytes = Vec::with_capacity(16 + body.len());
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
        bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&body);
        bytes
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 16 || &bytes[..8] != MAGIC {
            return Err(Error::Corrupt("primary run manifest: bad magic".into()));
        }
        let stored_crc = u32::from_le_bytes(bytes[8..12].try_into().expect("four bytes"));
        let body_len = u32::from_le_bytes(bytes[12..16].try_into().expect("four bytes")) as usize;
        let body = bytes
            .get(16..16usize.saturating_add(body_len))
            .filter(|_| bytes.len() == 16 + body_len)
            .ok_or_else(|| Error::Corrupt("primary run manifest: truncated body".into()))?;
        if crc32fast::hash(body) != stored_crc {
            return Err(Error::Corrupt("primary run manifest: crc mismatch".into()));
        }
        let manifest: Self = serde_json::from_slice(body)
            .map_err(|error| Error::Corrupt(format!("primary run manifest: {error}")))?;
        if manifest.format != FORMAT {
            return Err(Error::Corrupt(format!(
                "primary run manifest: unsupported format {}",
                manifest.format
            )));
        }
        if manifest.runs.iter().any(|run| {
            run.file.is_empty()
                || run.file.contains('/')
                || run.file.contains('\\')
                || run.file == "."
                || run.file == ".."
        }) {
            return Err(Error::Corrupt(
                "primary run manifest: invalid run filename".into(),
            ));
        }
        Ok(manifest)
    }

    pub(crate) fn load(indexes_dir: &Path, expected_generation: u64) -> Result<Self> {
        let manifest = read_manifest(&indexes_dir.join(PRIMARY_RUN_MANIFEST))?;
        if manifest.generation == expected_generation {
            Ok(manifest)
        } else {
            Err(Error::Corrupt(
                "primary run manifest: stale generation".into(),
            ))
        }
    }

    pub(crate) fn publish(&self, indexes_dir: &Path) -> Result<()> {
        fs::create_dir_all(indexes_dir)?;
        let tmp = indexes_dir.join(PRIMARY_RUN_MANIFEST_TMP);
        let current = indexes_dir.join(PRIMARY_RUN_MANIFEST);
        let mut file = File::create(&tmp)?;
        file.write_all(&self.encode())?;
        file.sync_all()?;
        // This manifest is disposable: one atomic replacement is sufficient.
        // A damaged file is rebuilt from canonical segments, so keeping a
        // second full run set would double disk residency after promotions.
        fs::rename(&tmp, &current)?;
        fsync_dir(indexes_dir)
    }

    pub(crate) fn referenced_files(indexes_dir: &Path) -> Vec<String> {
        read_manifest(&indexes_dir.join(PRIMARY_RUN_MANIFEST))
            .into_iter()
            .flat_map(|manifest| manifest.runs)
            .map(|run| run.file)
            .collect()
    }
}

impl DerivedRunManifest {
    pub(crate) fn new(
        kind: DerivedRunKind,
        table: &str,
        column: &str,
        generation: u64,
        runs: Vec<DerivedRunMeta>,
        aux: [u64; 2],
    ) -> Self {
        Self {
            format: FORMAT,
            kind,
            table: table.to_owned(),
            column: column.to_owned(),
            generation,
            runs,
            aux,
        }
    }

    fn encode(&self) -> Vec<u8> {
        encode_envelope(DERIVED_MAGIC, self)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let manifest: Self = decode_envelope(DERIVED_MAGIC, bytes, "derived run manifest")?;
        if manifest.format != FORMAT {
            return Err(Error::Corrupt(format!(
                "derived run manifest: unsupported format {}",
                manifest.format
            )));
        }
        if manifest.table.is_empty()
            || manifest.column.is_empty()
            || manifest.runs.iter().any(|run| !valid_filename(&run.file))
        {
            return Err(Error::Corrupt(
                "derived run manifest: invalid identity or filename".into(),
            ));
        }
        Ok(manifest)
    }

    pub(crate) fn load(
        path: &Path,
        kind: DerivedRunKind,
        table: &str,
        column: &str,
        expected_generation: u64,
    ) -> Result<Self> {
        let manifest = Self::decode(&fs::read(path)?)?;
        if manifest.kind != kind
            || manifest.table != table
            || manifest.column != column
            || manifest.generation != expected_generation
        {
            return Err(Error::Corrupt(
                "derived run manifest: stale or mismatched identity".into(),
            ));
        }
        Ok(manifest)
    }

    pub(crate) fn publish(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| Error::InvalidArgument("run manifest has no parent".into()))?;
        fs::create_dir_all(parent)?;
        let tmp = path.with_extension("runs.tmp");
        let mut file = File::create(&tmp)?;
        file.write_all(&self.encode())?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        fsync_dir(parent)
    }

    pub(crate) fn referenced_files(
        path: &Path,
        kind: DerivedRunKind,
        table: &str,
        column: &str,
    ) -> Vec<String> {
        Self::decode(&fs::read(path).unwrap_or_default())
            .ok()
            .filter(|manifest| {
                manifest.kind == kind && manifest.table == table && manifest.column == column
            })
            .into_iter()
            .flat_map(|manifest| manifest.runs)
            .map(|run| run.file)
            .collect()
    }
}

fn valid_filename(file: &str) -> bool {
    !file.is_empty() && !file.contains('/') && !file.contains('\\') && file != "." && file != ".."
}

fn encode_envelope(magic: &[u8; 8], value: &impl Serialize) -> Vec<u8> {
    let body = serde_json::to_vec(value).expect("run manifest always serializes");
    let mut bytes = Vec::with_capacity(16 + body.len());
    bytes.extend_from_slice(magic);
    bytes.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
    bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&body);
    bytes
}

fn decode_envelope<T: for<'de> Deserialize<'de>>(
    magic: &[u8; 8],
    bytes: &[u8],
    label: &str,
) -> Result<T> {
    if bytes.len() < 16 || &bytes[..8] != magic {
        return Err(Error::Corrupt(format!("{label}: bad magic")));
    }
    let stored_crc = u32::from_le_bytes(bytes[8..12].try_into().expect("four bytes"));
    let body_len = u32::from_le_bytes(bytes[12..16].try_into().expect("four bytes")) as usize;
    let body = bytes
        .get(16..16usize.saturating_add(body_len))
        .filter(|_| bytes.len() == 16 + body_len)
        .ok_or_else(|| Error::Corrupt(format!("{label}: truncated body")))?;
    if crc32fast::hash(body) != stored_crc {
        return Err(Error::Corrupt(format!("{label}: crc mismatch")));
    }
    serde_json::from_slice(body).map_err(|error| Error::Corrupt(format!("{label}: {error}")))
}

fn read_manifest(path: &Path) -> Result<PrimaryRunManifest> {
    PrimaryRunManifest::decode(&fs::read(path)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_atomic_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let first = PrimaryRunManifest::new(
            7,
            vec![PrimaryRunMeta {
                file: "primary.pidx".into(),
                level: 7,
                bytes: 42,
                generation: 7,
            }],
        );
        first.publish(dir.path()).unwrap();
        let second = PrimaryRunManifest::new(8, Vec::new());
        second.publish(dir.path()).unwrap();
        assert!(PrimaryRunManifest::load(dir.path(), 7).is_err());
        assert!(PrimaryRunManifest::load(dir.path(), 8).is_ok());
        assert!(PrimaryRunManifest::load(dir.path(), 9).is_err());
    }

    #[test]
    fn rejects_traversal_filenames() {
        let manifest = PrimaryRunManifest::new(
            1,
            vec![PrimaryRunMeta {
                file: "../segment".into(),
                level: 0,
                bytes: 1,
                generation: 1,
            }],
        );
        assert!(PrimaryRunManifest::decode(&manifest.encode()).is_err());
    }

    #[test]
    fn derived_manifest_checks_identity_generation_and_crc() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("abc.sidx.runs");
        let manifest = DerivedRunManifest::new(
            DerivedRunKind::Secondary,
            "items",
            "group",
            9,
            vec![DerivedRunMeta {
                file: "abc-L0-run.sidx.run".into(),
                level: 0,
                bytes: 12,
                generation: 9,
            }],
            [0, 0],
        );
        manifest.publish(&path).unwrap();
        assert!(
            DerivedRunManifest::load(&path, DerivedRunKind::Secondary, "items", "group", 9).is_ok()
        );
        assert!(
            DerivedRunManifest::load(&path, DerivedRunKind::Text, "items", "group", 9).is_err()
        );
        let mut bytes = fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        fs::write(&path, bytes).unwrap();
        assert!(
            DerivedRunManifest::load(&path, DerivedRunKind::Secondary, "items", "group", 9)
                .is_err()
        );
    }
}
