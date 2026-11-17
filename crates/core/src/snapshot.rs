//! List snapshots: stale-while-revalidate persisted to disk. The last home, detail, season, person, favorites and
//! first library page survive restarts, so a page draws from them at once and is replaced in place when fresh data
//! arrives. Only a cache: failed reads / writes or a wipe never break anything.
//!
//! One JSON file per key (named by a hash of the key, the key stored inside to tell collisions apart), written to
//! a temp file and renamed. Over [`LIMIT`] in total the least recently written go first. Calls block on disk: run
//! them off the async threads (the app does, see `App::snap_read` / `App::snap_write`).

use crate::lock;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Total bytes across snapshots. A first library page is ~100 KB, a detail page less: room for a hundred pages
/// while reading one stays cheap.
pub const LIMIT: u64 = 16 * 1024 * 1024;

/// Per file: when written (ms since the epoch; unique and increasing within a run) and its size.
#[derive(Default)]
struct Index {
    files: HashMap<String, (u64, u64)>,
    /// The newest write time handed out: equal clock readings still order writes.
    clock: u64,
}

pub struct Snapshots {
    dir: PathBuf,
    limit: u64,
    /// Read from the directory on first use, then kept up to date; the lock also serializes writes.
    index: Mutex<Option<Index>>,
}

#[derive(Serialize)]
struct EntryRef<'a, T> {
    key: &'a str,
    value: &'a T,
}

#[derive(serde::Deserialize)]
struct Entry<T> {
    key: String,
    value: T,
}

/// File name for a key: 64-bit FNV-1a (stable across builds, unlike std's hasher) in hex.
fn file_name(key: &str) -> String {
    let h = key.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    format!("{h:016x}.json")
}

/// One of our files (a snapshot or a temp file of one): clearing deletes only these.
fn ours(name: &str) -> bool {
    let stem = name
        .strip_suffix(".json")
        .or_else(|| name.strip_suffix(".json.tmp"));
    stem.is_some_and(|s| s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

impl Snapshots {
    /// Snapshots in `dir` (created on the first write).
    pub fn new(dir: PathBuf) -> Self {
        Self::with_limit(dir, LIMIT)
    }

    fn with_limit(dir: PathBuf, limit: u64) -> Self {
        Self {
            dir,
            limit,
            index: Mutex::new(None),
        }
    }

    /// Snapshot files on disk, with their write time and size.
    fn scan(dir: &Path) -> Index {
        let mut ix = Index::default();
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".json") || !ours(&name) {
                continue;
            }
            let Ok(m) = e.metadata() else { continue };
            let at = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
            ix.clock = ix.clock.max(at);
            ix.files.insert(name, (at, m.len()));
        }
        ix
    }

    fn with_index<R>(&self, f: impl FnOnce(&mut Index) -> R) -> R {
        let mut g = lock(&self.index);
        f(g.get_or_insert_with(|| Self::scan(&self.dir)))
    }

    /// The value stored under `key`; `None` if there is none or it no longer reads as `T`.
    pub fn read<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        let bytes = std::fs::read(self.dir.join(file_name(key))).ok()?;
        let e: Entry<T> = serde_json::from_slice(&bytes).ok()?;
        (e.key == key).then_some(e.value)
    }

    /// Stores `value` under `key`; over the total limit, evicts the least recently written (never this one).
    /// An entry over a quarter of the limit is not stored: it would evict everything else.
    pub fn write<T: Serialize>(&self, key: &str, value: &T) {
        let Ok(raw) = serde_json::to_vec(&EntryRef { key, value }) else {
            return;
        };
        let size = raw.len() as u64;
        if size > self.limit / 4 {
            return;
        }
        let name = file_name(key);
        self.with_index(|ix| {
            let path = self.dir.join(&name);
            let tmp = self.dir.join(format!("{name}.tmp"));
            let written = std::fs::create_dir_all(&self.dir)
                .and_then(|()| std::fs::write(&tmp, &raw))
                .and_then(|()| std::fs::rename(&tmp, &path));
            if let Err(e) = written {
                log::debug!("写列表快照失败：{e}");
                let _ = std::fs::remove_file(&tmp);
                return;
            }
            ix.clock = now_ms().max(ix.clock + 1);
            ix.files.insert(name.clone(), (ix.clock, size));
            let mut total: u64 = ix.files.values().map(|(_, n)| n).sum();
            let mut by_age: Vec<(u64, String)> = ix
                .files
                .iter()
                .map(|(k, (at, _))| (*at, k.clone()))
                .collect();
            by_age.sort();
            for (_, old) in by_age {
                if total <= self.limit {
                    break;
                }
                if old == name {
                    continue;
                }
                let _ = std::fs::remove_file(self.dir.join(&old));
                if let Some((_, n)) = ix.files.remove(&old) {
                    total -= n;
                }
            }
        });
    }

    /// Bytes used (cache management).
    pub fn size(&self) -> u64 {
        self.with_index(|ix| ix.files.values().map(|(_, n)| n).sum())
    }

    /// Drops every snapshot (cache management). Only our own files go, whatever else is in the directory.
    pub fn clear(&self) {
        self.with_index(|ix| {
            for e in std::fs::read_dir(&self.dir).into_iter().flatten().flatten() {
                if ours(&e.file_name().to_string_lossy()) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
            *ix = Index::default();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("embymini-snap-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// Bytes `write(key, value)` puts on disk.
    fn stored(key: &str, value: &Value) -> u64 {
        serde_json::to_vec(&EntryRef { key, value }).unwrap().len() as u64
    }

    #[test]
    fn reads_back_what_was_written() {
        let dir = tmpdir("rw");
        let s = Snapshots::new(dir.clone());
        s.write("a", &json!({ "x": 1 }));
        assert_eq!(s.read::<Value>("a"), Some(json!({ "x": 1 })), "written");
        assert_eq!(s.read::<Value>("nope"), None, "never written");
        assert_eq!(s.read::<Vec<u8>>("a"), None, "another type");
        // A new instance (the next run) finds it on disk
        let again = Snapshots::new(dir.clone());
        assert_eq!(
            again.read::<Value>("a"),
            Some(json!({ "x": 1 })),
            "after restart"
        );
        assert_eq!(
            again.size(),
            stored("a", &json!({ "x": 1 })),
            "size from disk"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Over the total limit the least recently written goes and the new one stays; a rewrite counts as recent.
    #[test]
    fn evicts_least_recently_written() {
        let dir = tmpdir("evict");
        let big = Value::String("x".repeat(700));
        let one = stored("k1", &big);
        // Four fit, a fifth evicts the oldest
        let s = Snapshots::with_limit(dir.clone(), one * 4 + one / 2);
        for k in ["k1", "k2", "k3", "k4"] {
            s.write(k, &big);
        }
        s.write("k2", &big); // just rewritten, so it must not go first
        s.write("k5", &big);
        assert_eq!(
            s.read::<Value>("k1"),
            None,
            "the least recently written is evicted"
        );
        for k in ["k2", "k3", "k4", "k5"] {
            assert_eq!(s.read::<Value>(k).as_ref(), Some(&big), "{k} kept");
        }
        assert_eq!(s.size(), one * 4, "usage after eviction");
        let files = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(files, 4, "evicted file deleted, no temp files left");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn entry_over_a_quarter_of_the_limit_is_skipped() {
        let dir = tmpdir("huge");
        let s = Snapshots::with_limit(dir.clone(), 4000);
        s.write("huge", &"x".repeat(1100));
        assert_eq!(s.read::<String>("huge"), None, "not stored");
        assert_eq!(s.size(), 0, "nothing counted");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// An unwritable directory or corrupt data never panics; a hash collision reads as missing.
    #[test]
    fn failures_are_quiet() {
        let dir = tmpdir("bad");
        std::fs::create_dir_all(&dir).unwrap();
        let blocker = dir.join("file");
        std::fs::write(&blocker, "").unwrap();
        let s = Snapshots::new(blocker.join("snap"));
        s.write("a", &1);
        assert_eq!(s.read::<i32>("a"), None, "nothing stored under a file");
        assert_eq!(s.size(), 0, "nothing counted");

        let s = Snapshots::new(dir.clone());
        std::fs::write(dir.join(file_name("bad")), "{not json").unwrap();
        assert_eq!(s.read::<Value>("bad"), None, "corrupt");
        std::fs::write(dir.join(file_name("other")), r#"{"key":"else","value":1}"#).unwrap();
        assert_eq!(s.read::<i32>("other"), None, "another key in the file");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Usage is the bytes on disk; clearing removes every snapshot and only snapshots.
    #[test]
    fn usage_and_clear() {
        let dir = tmpdir("clear");
        let s = Snapshots::new(dir.clone());
        s.write("a", &json!("xx"));
        s.write("b", &json!({ "y": 2 }));
        assert_eq!(
            s.size(),
            stored("a", &json!("xx")) + stored("b", &json!({ "y": 2 })),
            "usage"
        );
        std::fs::write(dir.join("keep.txt"), "").unwrap();
        s.clear();
        assert_eq!(s.size(), 0, "cleared");
        assert_eq!(s.read::<Value>("a"), None, "nothing reads back");
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(left, ["keep.txt"], "foreign files stay");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn only_our_names_are_ours() {
        assert!(
            ours(&file_name("a")) && ours(&format!("{}.tmp", file_name("a"))),
            "ours"
        );
        for other in [
            "keep.txt",
            "0123.json",
            "zzzzzzzzzzzzzzzz.json",
            "config.json",
        ] {
            assert!(!ours(other), "{other}");
        }
    }
}
