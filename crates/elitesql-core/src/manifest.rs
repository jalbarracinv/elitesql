use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::{Error, Result};
use crate::schema::{Catalog, FORMAT_VERSION};

pub(crate) const MANIFEST_FILE: &str = "manifest";
pub(crate) const MANIFEST_PREV_FILE: &str = "manifest.prev";
const MANIFEST_TMP_FILE: &str = "manifest.tmp";
const MANIFEST_PREV_TMP_FILE: &str = "manifest.prev.tmp";
const MAGIC: &[u8; 8] = b"ESQLMANI";

#[cfg(test)]
static FAIL_NEXT_PREVIOUS_DIR_SYNC: AtomicBool = AtomicBool::new(false);

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
    /// Highest durable integer identity allocated per table. A default keeps
    /// manifests written before identity support readable.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub identity_high_water: BTreeMap<String, i64>,
    /// Schema generation that describes these segments. Older manifests did
    /// not embed it and are upgraded on the first writable open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog: Option<Catalog>,
}

pub(crate) enum PublishOutcome {
    Complete,
    /// The namespace replacement completed, but directory durability could
    /// not be established. Callers must adopt the published generation and
    /// fence further writes until the database is reopened.
    SyncFailed(std::io::Error),
}

impl Manifest {
    pub fn initial() -> Self {
        Manifest {
            format_version: FORMAT_VERSION,
            committed_version: 0,
            segments: Vec::new(),
            wal_id: 1,
            identity_high_water: BTreeMap::new(),
            catalog: Some(Catalog::new()),
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
        if let Some(catalog) = &manifest.catalog {
            catalog.validate()?;
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
    pub fn publish(&self, dir: &Path) -> Result<PublishOutcome> {
        let tmp = dir.join(MANIFEST_TMP_FILE);
        let current = dir.join(MANIFEST_FILE);
        write_synced(&tmp, &self.encode())?;
        if current.exists() {
            fs::rename(&current, dir.join(MANIFEST_PREV_FILE))?;
        }
        if let Err(error) = fs::rename(&tmp, &current) {
            // The new generation was not published. Restore the old primary
            // when possible so readers do not have to depend on fallback
            // healing after an ordinary pre-publication error.
            let previous = dir.join(MANIFEST_PREV_FILE);
            if previous.exists() {
                let _ = fs::rename(&previous, &current);
                let _ = fsync_dir(dir);
            }
            return Err(error.into());
        }
        if let Err(error) = fsync_dir(dir) {
            return Ok(PublishOutcome::SyncFailed(as_io(error)));
        }
        self.refresh_previous(dir)
    }

    /// Repair after opening through `manifest.prev`: the primary manifest is
    /// corrupt, so it must NOT be rotated into `manifest.prev` (that would
    /// destroy the only good copy). Delete it first, then write a fresh
    /// primary; `manifest.prev` stays untouched as the fallback throughout.
    pub fn heal(&self, dir: &Path) -> Result<PublishOutcome> {
        let tmp = dir.join(MANIFEST_TMP_FILE);
        let current = dir.join(MANIFEST_FILE);
        match fs::remove_file(&current) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        write_synced(&tmp, &self.encode())?;
        fs::rename(&tmp, &current)?;
        if let Err(error) = fsync_dir(dir) {
            return Ok(PublishOutcome::SyncFailed(as_io(error)));
        }
        self.refresh_previous(dir)
    }

    pub fn previous(dir: &Path) -> Option<Manifest> {
        read_and_decode(&dir.join(MANIFEST_PREV_FILE)).ok()
    }

    /// Once the primary generation is durable, make the fallback an identical
    /// redundant copy. Until this finishes, the old fallback remains valid.
    pub fn refresh_previous(&self, dir: &Path) -> Result<PublishOutcome> {
        let tmp = dir.join(MANIFEST_PREV_TMP_FILE);
        if let Err(error) = write_synced(&tmp, &self.encode()) {
            return Ok(PublishOutcome::SyncFailed(as_io(error)));
        }
        if let Err(error) = fs::rename(&tmp, dir.join(MANIFEST_PREV_FILE)) {
            return Ok(PublishOutcome::SyncFailed(error));
        }
        #[cfg(test)]
        if FAIL_NEXT_PREVIOUS_DIR_SYNC.swap(false, Ordering::AcqRel) {
            return Ok(PublishOutcome::SyncFailed(std::io::Error::other(
                "injected manifest fallback directory sync failure",
            )));
        }
        Ok(match fsync_dir(dir) {
            Ok(()) => PublishOutcome::Complete,
            Err(error) => PublishOutcome::SyncFailed(as_io(error)),
        })
    }
}

#[cfg(test)]
pub(crate) fn fail_next_previous_dir_sync_for_test() {
    FAIL_NEXT_PREVIOUS_DIR_SYNC.store(true, Ordering::Release);
}

fn as_io(error: Error) -> std::io::Error {
    match error {
        Error::Io(error) => error,
        error => std::io::Error::other(error.to_string()),
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
