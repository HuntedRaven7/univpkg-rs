use std::collections::HashMap;
use std::fs;
use std::io;

use serde::{Deserialize, Serialize};

use crate::store::Store;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LockEntry {
    pub package: String,
    pub version: String,
    pub architecture: String,
    pub sha256: String,
    pub base: String,
    pub kind: String,
}

#[derive(Default, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Lockfile {
    packages: HashMap<String, LockEntry>,
}

impl Lockfile {
    pub fn path() -> io::Result<std::path::PathBuf> {
        Ok(Store::root()?.join("lock.json"))
    }

    pub fn load() -> Lockfile {
        Self::load_result().unwrap_or_default()
    }

    fn load_result() -> io::Result<Lockfile> {
        let path = Self::path()?;
        let text = fs::read_to_string(&path)?;
        serde_json::from_str(&text).map_err(io::Error::other)
    }

    pub fn save(&self) -> io::Result<()> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&self.packages).map_err(io::Error::other)?;
        fs::write(&path, json)
    }

    pub fn get(&self, package: &str, arch: &str) -> Option<&LockEntry> {
        self.packages.get(&key(package, arch))
    }

    pub fn set(&mut self, entry: LockEntry) {
        let k = key(&entry.package, &entry.architecture);
        self.packages.insert(k, entry);
    }

    pub fn remove(&mut self, package: &str, arch: &str) {
        self.packages.remove(&key(package, arch));
    }

    pub fn is_empty(&self) -> bool {
        self.packages.is_empty()
    }

    pub fn entries(&self) -> Vec<&LockEntry> {
        let mut out: Vec<&LockEntry> = self.packages.values().collect();
        out.sort_by(|a, b| {
            a.package
                .cmp(&b.package)
                .then(a.architecture.cmp(&b.architecture))
        });
        out
    }
}

fn key(package: &str, arch: &str) -> String {
    format!("{package}:{arch}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_lookup() {
        let mut lock = Lockfile::default();
        lock.set(LockEntry {
            package: "hello".into(),
            version: "1.0".into(),
            architecture: "amd64".into(),
            sha256: "deadbeef".into(),
            base: "http://example.invalid".into(),
            kind: "deb".into(),
        });
        assert!(lock.get("hello", "amd64").is_some());
        assert!(lock.get("hello", "i386").is_none());
        lock.remove("hello", "amd64");
        assert!(lock.is_empty());
    }
}
