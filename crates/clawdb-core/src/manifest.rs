use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use crate::error::{Error, Result};
use crate::schema::FORMAT_VERSION;

pub(crate) const MANIFEST_FILE: &str = "manifest";
pub(crate) const MANIFEST_PREV_FILE: &str = "manifest.prev";
const MANIFEST_TMP_FILE: &str = "manifest.tmp";
const MAGIC: &[u8; 8] = b"CLAWMANI";

// On-disk layout: 8-byte magic, u32 crc32 of the JSON body, u32 body length,
// JSON body. The manifest is the atomic pointer to the visible state; it is
// only ever replaced via temp-file + rename, with the previous manifest kept
// as `manifest.prev` for the recovery chain.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SegmentMeta {
    pub id: u32,
    /// Valid byte length. Bytes past this point are ignored on open.
    pub len: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Manifest {
    pub format_version: u32,
    /// Highest commit version fully contained in the listed segments.
    /// Commits above this watermark live in the WAL and are replayed on open.
    pub committed_version: u64,
    pub segments: Vec<SegmentMeta>,
    /// Active WAL file id. WAL files with a different id are obsolete.
    pub wal_id: u32,
}

impl Manifest {
    pub fn initial() -> Self {
        Manifest {
            format_version: FORMAT_VERSION,
            committed_version: 0,
            segments: Vec::new(),
            wal_id: 1,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let json = serde_json::to_vec_pretty(self).expect("manifest always serializes");
        let mut buf = Vec::with_capacity(json.len() + 16);
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&crc32fast::hash(&json).to_le_bytes());
        buf.extend_from_slice(&(json.len() as u32).to_le_bytes());
        buf.extend_from_slice(&json);
        buf
    }

    pub fn decode(bytes: &[u8]) -> Result<Manifest> {
        if bytes.len() < 16 || &bytes[..8] != MAGIC {
            return Err(Error::Corrupt("manifest: bad magic".into()));
        }
        let stored_crc = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        let len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        let body = bytes
            .get(16..16 + len)
            .ok_or_else(|| Error::Corrupt("manifest: truncated body".into()))?;
        if crc32fast::hash(body) != stored_crc {
            return Err(Error::Corrupt("manifest: crc mismatch".into()));
        }
        let manifest: Manifest = serde_json::from_slice(body)
            .map_err(|e| Error::Corrupt(format!("manifest: invalid json: {e}")))?;
        if manifest.format_version != FORMAT_VERSION {
            return Err(Error::Corrupt(format!(
                "unsupported format_version {} (expected {FORMAT_VERSION})",
                manifest.format_version
            )));
        }
        Ok(manifest)
    }

    /// Load the manifest, falling back to `manifest.prev` per the recovery
    /// chain. Returns the manifest and whether the fallback was used.
    pub fn load(dir: &Path) -> Result<(Manifest, bool)> {
        match read_and_decode(&dir.join(MANIFEST_FILE)) {
            Ok(m) => Ok((m, false)),
            Err(primary_err) => match read_and_decode(&dir.join(MANIFEST_PREV_FILE)) {
                Ok(m) => Ok((m, true)),
                Err(_) => Err(Error::Corrupt(format!(
                    "no valid manifest (primary: {primary_err})"
                ))),
            },
        }
    }

    /// Publish atomically: write temp file (fsync), rotate the current
    /// manifest to `manifest.prev`, rename temp into place, fsync the dir.
    /// A crash between the two renames leaves a valid `manifest.prev`,
    /// which `load` falls back to.
    pub fn publish(&self, dir: &Path) -> Result<()> {
        let tmp = dir.join(MANIFEST_TMP_FILE);
        let current = dir.join(MANIFEST_FILE);
        write_synced(&tmp, &self.encode())?;
        if current.exists() {
            fs::rename(&current, dir.join(MANIFEST_PREV_FILE))?;
        }
        fs::rename(&tmp, &current)?;
        fsync_dir(dir)
    }

    /// Repair after opening through `manifest.prev`: the primary manifest is
    /// corrupt, so it must NOT be rotated into `manifest.prev` (that would
    /// destroy the only good copy). Delete it first, then write a fresh
    /// primary; `manifest.prev` stays untouched as the fallback throughout.
    pub fn heal(&self, dir: &Path) -> Result<()> {
        let tmp = dir.join(MANIFEST_TMP_FILE);
        let current = dir.join(MANIFEST_FILE);
        match fs::remove_file(&current) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        write_synced(&tmp, &self.encode())?;
        fs::rename(&tmp, &current)?;
        fsync_dir(dir)
    }
}

fn read_and_decode(path: &Path) -> Result<Manifest> {
    let bytes = fs::read(path)?;
    Manifest::decode(&bytes)
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = File::create(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

pub(crate) fn fsync_dir(dir: &Path) -> Result<()> {
    File::open(dir)?.sync_all()?;
    Ok(())
}
