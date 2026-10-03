//! What kiln remembers between builds (`.kiln/state.json`): for every step the hash of
//! everything it depended on when it last succeeded, and a cache of file content hashes keyed by
//! modification time and size, so unchanged files are not read again.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Stamp {
    pub mtime_ns: u128,
    pub size: u64,
    pub hash: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct StepRecord {
    pub key: String,
    pub command: String,
    /// Every input (declared and discovered) and its content hash.
    pub inputs: BTreeMap<String, String>,
    pub outputs: BTreeMap<String, String>,
    /// Inputs reported by the depfile.
    pub discovered: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub files: HashMap<String, Stamp>,
    /// By the step's first output.
    #[serde(default)]
    pub steps: HashMap<String, StepRecord>,
}

pub fn key_path(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

impl State {
    pub fn load(path: &Path) -> State {
        std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    /// Writes atomically (temporary file, then rename).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(tmp, path)
    }
}

/// Content hashes of files, reusing the cached hash while modification time and size match.
pub struct Hasher {
    root: PathBuf,
    cache: Mutex<HashMap<String, Stamp>>,
    pub files_read: std::sync::atomic::AtomicUsize,
    /// A cache entry was added, changed or removed.
    pub dirty: std::sync::atomic::AtomicBool,
}

impl Hasher {
    pub fn new(root: &Path, cache: HashMap<String, Stamp>) -> Hasher {
        Hasher { root: root.to_path_buf(), cache: Mutex::new(cache), files_read: Default::default(), dirty: Default::default() }
    }

    /// The hash of a file's content, `None` if it does not exist.
    pub fn hash(&self, rel: &Path) -> Option<String> {
        let full = self.root.join(rel);
        let meta = std::fs::metadata(&full).ok()?;
        if !meta.is_file() {
            return None;
        }
        let mtime_ns = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0);
        let size = meta.len();
        let k = key_path(rel);
        if let Some(s) = self.cache.lock().unwrap().get(&k) {
            if s.mtime_ns == mtime_ns && s.size == size {
                return Some(s.hash.clone());
            }
        }
        let bytes = std::fs::read(&full).ok()?;
        self.files_read.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let hash = blake3::hash(&bytes).to_hex().to_string();
        self.cache.lock().unwrap().insert(k, Stamp { mtime_ns, size, hash: hash.clone() });
        self.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        Some(hash)
    }

    pub fn forget(&self, rel: &Path) {
        self.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        self.cache.lock().unwrap().remove(&key_path(rel));
    }

    pub fn cache_mut(&self) -> std::sync::MutexGuard<'_, HashMap<String, Stamp>> {
        self.cache.lock().unwrap()
    }
}

/// The key of a step: its command, its outputs' names, and every input with its content hash.
pub fn step_key(command: &str, outputs: &[PathBuf], inputs: &BTreeMap<String, String>) -> String {
    let mut h = blake3::Hasher::new();
    h.update(command.as_bytes());
    h.update(b"\0");
    for o in outputs {
        h.update(key_path(o).as_bytes());
        h.update(b"\0");
    }
    for (p, hash) in inputs {
        h.update(p.as_bytes());
        h.update(b"\0");
        h.update(hash.as_bytes());
        h.update(b"\0");
    }
    h.finalize().to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_files_are_not_read_again_and_touching_keeps_the_hash() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a"), "hello").unwrap();
        let h = Hasher::new(d.path(), HashMap::new());
        let first = h.hash(Path::new("a")).unwrap();
        assert_eq!(h.hash(Path::new("a")).unwrap(), first);
        assert_eq!(h.files_read.load(std::sync::atomic::Ordering::Relaxed), 1);
        // Same content, new modification time: read again, same hash.
        let f = std::fs::File::options().write(true).open(d.path().join("a")).unwrap();
        f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5)).unwrap();
        drop(f);
        assert_eq!(h.hash(Path::new("a")).unwrap(), first);
        assert_eq!(h.files_read.load(std::sync::atomic::Ordering::Relaxed), 2);
        std::fs::write(d.path().join("a"), "hellO").unwrap();
        assert_ne!(h.hash(Path::new("a")).unwrap(), first);
        assert!(h.hash(Path::new("missing")).is_none());
    }

    #[test]
    fn keys_change_with_command_inputs_and_outputs() {
        let mut i = BTreeMap::new();
        i.insert("a.c".to_string(), "h1".to_string());
        let k = step_key("cc a.c", &[PathBuf::from("a.o")], &i);
        assert_eq!(k, step_key("cc a.c", &[PathBuf::from("a.o")], &i));
        assert_ne!(k, step_key("cc -O2 a.c", &[PathBuf::from("a.o")], &i));
        assert_ne!(k, step_key("cc a.c", &[PathBuf::from("b.o")], &i));
        i.insert("a.c".to_string(), "h2".to_string());
        assert_ne!(k, step_key("cc a.c", &[PathBuf::from("a.o")], &i));
    }

    #[test]
    fn state_round_trips_and_a_broken_file_starts_fresh() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join(".kiln/state.json");
        let mut s = State::default();
        s.steps.insert("a.o".into(), StepRecord { key: "k".into(), ..Default::default() });
        s.save(&p).unwrap();
        assert_eq!(State::load(&p).steps["a.o"].key, "k");
        std::fs::write(&p, "{broken").unwrap();
        assert!(State::load(&p).steps.is_empty());
    }
}
