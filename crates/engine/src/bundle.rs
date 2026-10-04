//! A `.rock` file: a zip with the code in `main.txt`, and the resources it uses
//! in a folder per kind (`samples/kick.mp3`, `envelopes/pluck.json`, ...), so
//! one file is the whole piece.
//!
//! The whole file is read into memory when it's opened. Evaluating code reads
//! resources from there, editors change them there, and saving writes it all
//! back. A plain text file is read too (as code without resources), and
//! becomes a zip when it's saved.

use std::collections::BTreeMap;
use std::io::{Cursor, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter};

use crate::resource::ResourceKind;

/// Where the code is, inside the zip.
pub const CODE: &str = "main.txt";

const KICK: &[u8] = include_bytes!("../assets/kick.mp3");

/// Every version of every file gets a new number, so caches can tell a changed
/// file from the one they have.
static VERSIONS: AtomicU64 = AtomicU64::new(1);

/// A file in a bundle.
#[derive(Clone, Debug)]
pub struct Entry {
    pub data: Arc<[u8]>,
    /// Unique to this version of this file.
    pub version: u64,
}

/// The resources of a `.rock` file, by their path inside it. Cloning it gives
/// another handle to the same files, so a change made through one (by an
/// editor) is seen through all (by the evaluator).
#[derive(Clone, Debug, Default)]
pub struct Bundle(Arc<Mutex<BTreeMap<String, Entry>>>);

impl Bundle {
    fn files(&self) -> MutexGuard<'_, BTreeMap<String, Entry>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The file at `path` (like "envelopes/pluck.json").
    pub fn get_path(&self, path: &str) -> Option<Entry> {
        self.files().get(path).cloned()
    }

    pub fn put_path(&self, path: &str, data: Vec<u8>) {
        let entry = Entry {
            data: data.into(),
            version: VERSIONS.fetch_add(1, Ordering::Relaxed),
        };
        self.files().insert(path.to_string(), entry);
    }

    /// A resource, by kind and name.
    pub fn get(&self, kind: ResourceKind, name: &str) -> Option<Entry> {
        self.get_path(&kind.bundle_path(name))
    }

    pub fn put(&self, kind: ResourceKind, name: &str, data: Vec<u8>) {
        self.put_path(&kind.bundle_path(name), data)
    }

    /// The paths of all files, sorted.
    pub fn paths(&self) -> Vec<String> {
        self.files().keys().cloned().collect()
    }

    /// A bundle with just `kick.mp3`: what a new file starts with, and what
    /// tests use.
    pub fn with_kick() -> Self {
        let bundle = Self::default();
        bundle.put(ResourceKind::Sample, "kick.mp3", KICK.to_vec());
        bundle
    }
}

/// Read a `.rock` file: its code and its resources. `Ok(None)` if there's no
/// file there yet.
pub fn open(path: &Path) -> Result<Option<(String, Bundle)>, String> {
    let fail = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(fail(&e)),
    };
    let bundle = Bundle::default();
    if !bytes.starts_with(b"PK\x03\x04") {
        // Plain text, from before `.rock` files were zips.
        let code = String::from_utf8(bytes).map_err(|_| fail(&"neither a zip nor text"))?;
        return Ok(Some((code, bundle)));
    }

    let mut zip = ZipArchive::new(Cursor::new(bytes)).map_err(|e| fail(&e))?;
    let mut code = None;
    for i in 0..zip.len() {
        let mut file = zip.by_index(i).map_err(|e| fail(&e))?;
        if !file.is_file() {
            continue;
        }
        let mut data = Vec::with_capacity(file.size() as usize);
        file.read_to_end(&mut data).map_err(|e| fail(&e))?;
        if file.name() == CODE {
            code = Some(String::from_utf8(data).map_err(|_| fail(&"main.txt isn't text"))?);
        } else {
            bundle.put_path(file.name(), data);
        }
    }
    let code = code.ok_or_else(|| fail(&"no main.txt in it"))?;
    Ok(Some((code, bundle)))
}

/// Write a `.rock` file. It's written next to the old one first and then moved
/// over it, so a failed save never leaves half a file.
pub fn save(path: &Path, code: &str, bundle: &Bundle) -> Result<(), String> {
    let fail = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    let bytes = to_zip(code, bundle).map_err(|e| fail(&e))?;
    let mut temp = path.as_os_str().to_owned();
    temp.push(".saving");
    std::fs::write(&temp, bytes).map_err(|e| fail(&e))?;
    std::fs::rename(&temp, path).map_err(|e| fail(&e))
}

fn to_zip(code: &str, bundle: &Bundle) -> zip::result::ZipResult<Vec<u8>> {
    // A fixed date, so saving the same content twice gives the same bytes.
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .last_modified_time(DateTime::default());
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    zip.start_file(CODE, options)?;
    zip.write_all(code.as_bytes())?;
    for (path, entry) in bundle.files().iter() {
        zip.start_file(path, options)?;
        zip.write_all(&entry.data)?;
    }
    Ok(zip.finish()?.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("rocktober-bundle-{}-{name}", std::process::id()))
    }

    #[test]
    fn round_trips() {
        let path = temp("song.rock");
        let bundle = Bundle::default();
        bundle.put(ResourceKind::Envelope, "pluck", b"{}".to_vec());
        bundle.put(ResourceKind::Sample, "drums/kick.wav", vec![1, 2, 3]);
        save(&path, "sample(\"drums/kick.wav\").play\n", &bundle).unwrap();
        let first = std::fs::read(&path).unwrap();

        let (code, opened) = open(&path).unwrap().unwrap();
        assert_eq!(code, "sample(\"drums/kick.wav\").play\n");
        assert_eq!(
            opened.paths(),
            ["envelopes/pluck.json", "samples/drums/kick.wav"]
        );
        let kick = opened.get(ResourceKind::Sample, "drums/kick.wav").unwrap();
        assert_eq!(&*kick.data, [1, 2, 3]);

        // Same content, same bytes.
        save(&path, &code, &opened).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), first);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn reads_plain_text_and_missing_files() {
        let path = temp("old.rock");
        std::fs::write(&path, "120.bpm\n").unwrap();
        let (code, bundle) = open(&path).unwrap().unwrap();
        assert_eq!(code, "120.bpm\n");
        assert!(bundle.paths().is_empty());
        std::fs::remove_file(&path).unwrap();
        assert!(open(&path).unwrap().is_none());
    }

    #[test]
    fn changes_are_shared_and_versioned() {
        let bundle = Bundle::default();
        let handle = bundle.clone();
        handle.put(ResourceKind::Modulation, "sweep", b"a".to_vec());
        let first = bundle.get(ResourceKind::Modulation, "sweep").unwrap();
        handle.put(ResourceKind::Modulation, "sweep", b"b".to_vec());
        let second = bundle.get(ResourceKind::Modulation, "sweep").unwrap();
        assert_eq!(&*second.data, b"b");
        assert_ne!(first.version, second.version);
    }
}
