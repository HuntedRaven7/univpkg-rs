use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::PathBuf;

use crate::resolve;
use crate::store::Store;
use crate::txn::Txn;

pub fn profiles_dir() -> io::Result<PathBuf> {
    Ok(Store::root()?.join("profiles"))
}

pub fn list() -> io::Result<Vec<String>> {
    let dir = profiles_dir()?;
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(&dir) {
        for e in entries.flatten() {
            if e.path().is_file() {
                out.push(e.file_name().to_string_lossy().into_owned());
            }
        }
    }
    out.sort();
    Ok(out)
}

pub fn save(name: &str, packages: &[String]) -> io::Result<()> {
    let dir = profiles_dir()?;
    fs::create_dir_all(&dir)?;
    let mut text = String::new();
    for p in packages {
        text.push_str(p.trim());
        text.push('\n');
    }
    fs::write(dir.join(name), text)
}

pub fn show(name: &str) -> io::Result<Vec<String>> {
    let path = profiles_dir()?.join(name);
    let text = fs::read_to_string(&path)?;
    Ok(parse(&text))
}

fn parse(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

pub fn remove(name: &str) -> io::Result<()> {
    let path = profiles_dir()?.join(name);
    if !path.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no profile '{name}'"),
        ));
    }
    fs::remove_file(path)
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ApplyResult {
    pub installed: usize,
    pub removed: usize,
}

pub fn apply(store: &Store, name: &str) -> io::Result<ApplyResult> {
    let desired_list = show(name)?;
    if desired_list.is_empty() {
        return Err(io::Error::other(format!("profile '{name}' is empty")));
    }
    let desired: HashSet<String> = desired_list.iter().cloned().collect();
    let installed_before = resolve::installed_packages(store);

    let mut txn = Txn::begin(store)?;
    let result = apply_in_txn(store, &desired_list, &desired, &installed_before, &mut txn);
    let outcome = match result {
        Ok(out) => {
            txn.lock().save()?;
            txn.commit()?;
            out
        }
        Err(e) => {
            txn.rollback();
            return Err(e);
        }
    };

    for p in resolve::installed_packages(store) {
        if desired.contains(&p.meta.package) {
            let _ = crate::link::link_package(store, &p.sp, &p.meta);
        }
    }
    Ok(outcome)
}

fn apply_in_txn(
    store: &Store,
    desired_list: &[String],
    desired: &HashSet<String>,
    installed_before: &[resolve::Installed],
    txn: &mut Txn,
) -> io::Result<ApplyResult> {
    let mut result = ApplyResult::default();

    for name in desired_list {
        if let Some(p) = installed_before.iter().find(|p| p.meta.package == name.as_str()) {
            txn.set_manual(&p.sp)?;
            continue;
        }
        if let Some(repo) = crate::repo::repo_for(name) {
            let out = crate::repo::install_in_txn(store, &repo, name, txn)?;
            if !out.is_empty() {
                result.installed += 1;
            }
        } else if let Some(repo) = crate::rpmrepo::repo_for(name) {
            let out = crate::rpmrepo::install_in_txn(store, &repo, name, txn)?;
            if !out.is_empty() {
                result.installed += 1;
            }
        } else {
            return Err(io::Error::other(format!(
                "package '{name}' is not available in any configured repo"
            )));
        }
    }

    let installed_after = resolve::installed_packages(store);
    let keep = keep_set(&installed_after, desired);
    for p in &installed_after {
        if !keep.contains(&p.meta.package) {
            txn.remove_store(&p.sp)?;
            result.removed += 1;
        }
    }
    Ok(result)
}

fn keep_set(installed: &[resolve::Installed], desired: &HashSet<String>) -> HashSet<String> {
    let mut keep: HashSet<String> = HashSet::new();
    let mut stack: Vec<String> = desired.iter().cloned().collect();
    while let Some(name) = stack.pop() {
        if !keep.insert(name.clone()) {
            continue;
        }
        for p in installed.iter().filter(|p| p.meta.package == name) {
            for group in &p.meta.depends {
                for dep in group {
                    if installed.iter().any(|q| q.meta.package == dep.package) {
                        stack.push(dep.package.clone());
                    }
                }
            }
        }
    }
    keep
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ignores_comments_and_blanks() {
        let text = "# comment\n\nhello\n  vim  \n# trailing\n";
        assert_eq!(parse(text), vec!["hello", "vim"]);
    }

    #[test]
    fn save_show_roundtrip() {
        let _g = crate::store::TEST_HOME_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir().join(format!("univ-profile-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join(".local/share/univ")).unwrap();
        unsafe { std::env::set_var("HOME", &tmp) };

        save("base", &["hello".into(), "vim".into()]).unwrap();
        assert_eq!(show("base").unwrap(), vec!["hello", "vim"]);
        assert_eq!(list().unwrap(), vec!["base"]);
        remove("base").unwrap();
        assert!(list().unwrap().is_empty());

        fs::remove_dir_all(&tmp).unwrap();
    }
}
