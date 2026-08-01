use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::PathBuf;

use crate::link;
use crate::lock::Lockfile;
use crate::store::{Store, StorePath};

enum Op {
    AddStorePath(StorePath),
    RemoveStorePath {
        sp: StorePath,
        store_trash: PathBuf,
        state_trash: PathBuf,
        has_store: bool,
        has_state: bool,
    },
    SetManual { sp: StorePath, was_auto: bool },
}

pub struct Txn {
    store: Store,
    ops: Vec<Op>,
    pre_existing: HashSet<StorePath>,
    lock: Lockfile,
    lock_snapshot: Lockfile,
    trash: PathBuf,
    done: bool,
}

impl Txn {
    pub fn begin(store: &Store) -> io::Result<Txn> {
        restore_stale_trash(store)?;
        let trash = Store::state_dir()?.join("trash").join(txn_id());
        let lock = Lockfile::load();
        let lock_snapshot = lock.clone();
        let pre_existing: HashSet<StorePath> = store.paths()?.into_iter().collect();
        Ok(Txn {
            store: store.clone(),
            ops: Vec::new(),
            pre_existing,
            lock,
            lock_snapshot,
            trash,
            done: false,
        })
    }

    pub fn lock(&mut self) -> &mut Lockfile {
        &mut self.lock
    }

    pub fn add_store(&mut self, sp: &StorePath) -> io::Result<()> {
        if !self.store.base().join(sp.to_string()).exists() {
            return Err(io::Error::other(format!(
                "store path {} does not exist",
                sp
            )));
        }
        if !self.pre_existing.contains(sp) {
            self.ops.push(Op::AddStorePath(sp.clone()));
        }
        Ok(())
    }

    pub fn remove_store(&mut self, sp: &StorePath) -> io::Result<()> {
        let state = Store::state_dir()?;
        let store_dir = self.store.base().join(sp.to_string());
        let state_dir = state.join(sp.to_string());

        let _ = link::remove_artifacts(sp);
        if let Ok(meta) = crate::deb::read_meta(sp) {
            self.lock.remove(&meta.package, &meta.architecture);
        }

        let store_trash = self.trash.join("store").join(sp.to_string());
        let state_trash = self.trash.join("state").join(sp.to_string());
        if let Some(parent) = store_trash.parent() {
            fs::create_dir_all(parent)?;
        }
        if let Some(parent) = state_trash.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut has_store = false;
        let mut has_state = false;
        if store_dir.exists() {
            fs::rename(&store_dir, &store_trash)?;
            has_store = true;
        }
        if state_dir.exists() {
            fs::rename(&state_dir, &state_trash)?;
            has_state = true;
        }
        if !has_store && !has_state {
            return Err(io::Error::other(format!(
                "no such store path {}",
                sp
            )));
        }

        self.ops.push(Op::RemoveStorePath {
            sp: sp.clone(),
            store_trash,
            state_trash,
            has_store,
            has_state,
        });
        Ok(())
    }

    pub fn set_manual(&mut self, sp: &StorePath) -> io::Result<()> {
        let was_auto = crate::store::is_auto(sp);
        crate::store::mark_manual(sp)?;
        self.ops.push(Op::SetManual {
            sp: sp.clone(),
            was_auto,
        });
        Ok(())
    }

    pub fn commit(&mut self) -> io::Result<()> {
        if self.done {
            return Ok(());
        }
        self.done = true;
        let _ = fs::remove_dir_all(&self.trash);
        Ok(())
    }

    pub fn rollback(&mut self) {
        if self.done {
            return;
        }
        self.done = true;
        self.rollback_inner();
    }

    fn rollback_inner(&mut self) {
        let state = match Store::state_dir() {
            Ok(s) => s,
            Err(_) => return,
        };

        for op in self.ops.iter().rev() {
            match op {
                Op::AddStorePath(sp) => {
                    let _ = link::remove_artifacts(sp);
                    let _ = fs::remove_dir_all(state.join(sp.to_string()));
                    let _ = fs::remove_dir_all(self.store.base().join(sp.to_string()));
                }
                Op::RemoveStorePath { .. } => {}
                Op::SetManual { sp, was_auto } => {
                    if *was_auto {
                        let _ = crate::store::mark_auto(sp);
                    }
                }
            }
        }

        for op in &self.ops {
            if let Op::RemoveStorePath {
                sp,
                store_trash,
                state_trash,
                has_store,
                has_state,
            } = op
            {
                if *has_store && store_trash.exists() {
                    let _ = fs::rename(store_trash, self.store.base().join(sp.to_string()));
                }
                if *has_state && state_trash.exists() {
                    let _ = fs::rename(state_trash, state.join(sp.to_string()));
                }
            }
        }

        for op in &self.ops {
            if let Op::RemoveStorePath { sp, .. } = op {
                relink(&self.store, sp);
            }
        }

        let _ = self.lock_snapshot.save();
        let _ = fs::remove_dir_all(&self.trash);
    }
}

impl Drop for Txn {
    fn drop(&mut self) {
        if !self.done {
            self.done = true;
            self.rollback_inner();
        }
    }
}

fn relink(store: &Store, sp: &StorePath) {
    if let Ok(meta) = crate::deb::read_meta(sp) {
        let _ = link::link_package(store, sp, &meta);
    }
}

fn txn_id() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
}

fn restore_stale_trash(store: &Store) -> io::Result<()> {
    let trash_root = Store::state_dir()?.join("trash");
    let entries = match fs::read_dir(&trash_root) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for entry in entries.flatten() {
        let txn_dir = entry.path();
        for sub in ["store", "state"] {
            let from_dir = txn_dir.join(sub);
            let dest_base = if sub == "store" {
                store.base().to_path_buf()
            } else {
                Store::state_dir()?
            };
            if let Ok(entries) = fs::read_dir(&from_dir) {
                for e in entries.flatten() {
                    let from = e.path();
                    let dest = dest_base.join(e.file_name());
                    if !dest.exists() {
                        let _ = fs::rename(&from, &dest);
                    }
                }
            }
        }
        let _ = fs::remove_dir_all(&txn_dir);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "univ-txn-{tag}-{}",
            std::process::id()
        ))
    }

    fn setup(
        tag: &str,
    ) -> (std::sync::MutexGuard<'static, ()>, Store) {
        let g = crate::store::TEST_HOME_LOCK.lock().unwrap();
        let tmp = tmp_home(tag);
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join(".local/univ/store")).unwrap();
        fs::create_dir_all(tmp.join(".local/univ/state")).unwrap();
        unsafe { std::env::set_var("HOME", &tmp) };
        let store = crate::store::test_store(&tmp.join(".local/univ/store"));
        (g, store)
    }

    fn make_path(store: &Store, sp: &StorePath) {
        let dir = store.base().join(sp.to_string());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("data"), "payload").unwrap();
    }

    fn sp(name: &str) -> StorePath {
        StorePath::parse(&format!("{}-{name}", "b".repeat(64))).unwrap()
    }

    #[test]
    fn add_store_rollback_removes_path() {
        let (_g, store) = setup("add");
        let s = sp("hello-1.0-amd64");

        let mut txn = Txn::begin(&store).unwrap();
        make_path(&store, &s);
        txn.add_store(&s).unwrap();
        assert!(store.base().join(s.to_string()).exists());

        txn.rollback();
        assert!(!store.base().join(s.to_string()).exists());
        let _ = fs::remove_dir_all(tmp_home("add"));
    }

    #[test]
    fn add_store_commit_keeps_path() {
        let (_g, store) = setup("addc");
        let s = sp("hello-1.0-amd64");
        make_path(&store, &s);

        let mut txn = Txn::begin(&store).unwrap();
        txn.add_store(&s).unwrap();
        txn.commit().unwrap();
        assert!(store.base().join(s.to_string()).exists());
        let _ = fs::remove_dir_all(tmp_home("addc"));
    }

    #[test]
    fn add_preexisting_path_is_left_alone_on_rollback() {
        let (_g, store) = setup("pre");
        let s = sp("hello-1.0-amd64");
        make_path(&store, &s);

        let mut txn = Txn::begin(&store).unwrap();
        txn.add_store(&s).unwrap();
        txn.rollback();
        assert!(store.base().join(s.to_string()).exists());
        let _ = fs::remove_dir_all(tmp_home("pre"));
    }

    #[test]
    fn remove_store_rollback_restores_path() {
        let (_g, store) = setup("rm");
        let s = sp("hello-1.0-amd64");
        make_path(&store, &s);
        let state = Store::state_dir().unwrap();
        fs::create_dir_all(state.join(s.to_string())).unwrap();
        fs::write(state.join(s.to_string()).join("meta"), "Package: hello\n").unwrap();

        let mut txn = Txn::begin(&store).unwrap();
        txn.remove_store(&s).unwrap();
        assert!(!store.base().join(s.to_string()).exists());
        assert!(!state.join(s.to_string()).exists());

        txn.rollback();
        assert!(store.base().join(s.to_string()).exists());
        assert!(state.join(s.to_string()).exists());
        assert!(store.base().join(s.to_string()).join("data").exists());
        let _ = fs::remove_dir_all(tmp_home("rm"));
    }

    #[test]
    fn remove_store_commit_deletes_trash() {
        let (_g, store) = setup("rmc");
        let s = sp("hello-1.0-amd64");
        make_path(&store, &s);

        let mut txn = Txn::begin(&store).unwrap();
        txn.remove_store(&s).unwrap();
        txn.commit().unwrap();
        assert!(!store.base().join(s.to_string()).exists());
        let trash_root = Store::state_dir().unwrap().join("trash");
        assert!(fs::read_dir(&trash_root).map(|mut it| it.next().is_none()).unwrap_or(true));
        let _ = fs::remove_dir_all(tmp_home("rmc"));
    }

    #[test]
    fn rollback_restores_lockfile_snapshot() {
        let (_g, store) = setup("lock");
        let mut lock = Lockfile::load();
        lock.set(crate::lock::LockEntry {
            package: "hello".into(),
            version: "1.0".into(),
            architecture: "amd64".into(),
            sha256: "old".into(),
            base: "http://example.invalid".into(),
            kind: "deb".into(),
        });
        lock.save().unwrap();

        let mut txn = Txn::begin(&store).unwrap();
        txn.lock().set(crate::lock::LockEntry {
            package: "hello".into(),
            version: "2.0".into(),
            architecture: "amd64".into(),
            sha256: "new".into(),
            base: "http://example.invalid".into(),
            kind: "deb".into(),
        });
        txn.rollback();

        let after = Lockfile::load();
        assert_eq!(after.get("hello", "amd64").unwrap().version, "1.0");
        let _ = fs::remove_dir_all(tmp_home("lock"));
    }

    #[test]
    fn stale_trash_is_restored_on_next_begin() {
        let (_g, store) = setup("stale");
        let s = sp("hello-1.0-amd64");
        make_path(&store, &s);

        let trash = Store::state_dir()
            .unwrap()
            .join("trash")
            .join("dead-txn-1");
        fs::create_dir_all(trash.join("store")).unwrap();
        fs::rename(
            store.base().join(s.to_string()),
            trash.join("store").join(s.to_string()),
        )
        .unwrap();
        assert!(!store.base().join(s.to_string()).exists());

        let _txn = Txn::begin(&store).unwrap();
        assert!(store.base().join(s.to_string()).exists());
        let _ = fs::remove_dir_all(tmp_home("stale"));
    }

    #[test]
    fn drop_without_commit_rolls_back() {
        let (_g, store) = setup("drop");
        let s = sp("hello-1.0-amd64");

        {
            let mut txn = Txn::begin(&store).unwrap();
            make_path(&store, &s);
            txn.add_store(&s).unwrap();
        }
        assert!(!store.base().join(s.to_string()).exists());
        let _ = fs::remove_dir_all(tmp_home("drop"));
    }
}
