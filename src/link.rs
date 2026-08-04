use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::deb::DebMeta;
use crate::store::{Store, StorePath};

const BIN_DIRS: &[&str] = &[
    "usr/bin",
    "usr/sbin",
    "usr/games",
    "bin",
    "sbin",
    "usr/local/bin",
    "usr/local/sbin",
];

const DESKTOP_SUFFIX: &str = " (univ)";

#[derive(Clone, Debug, Default)]
pub struct Linked {
    pub bin_links: Vec<PathBuf>,
    pub desktop_files: Vec<PathBuf>,
    pub icons: Vec<PathBuf>,
}

pub fn link_package(store: &Store, sp: &StorePath, meta: &DebMeta) -> io::Result<Linked> {
    let home = Store::home_dir()?;
    let kind = crate::nspawn::package_kind(sp);
    let store_path = store.base().join(sp.to_string());
    let bin_dir = home.join(".local").join("bin");
    let apps_dir = home.join(".local").join("share").join("applications");
    let icons_dir = home.join(".local").join("share").join("icons");
    let pixmaps_dir = home.join(".local").join("share").join("pixmaps");

    fs::create_dir_all(&bin_dir)?;
    fs::create_dir_all(&apps_dir)?;

    let mut linked = Linked::default();
    let mut manifest: Vec<(char, PathBuf, PathBuf)> = Vec::new();
    let mut wrapped: HashMap<String, PathBuf> = HashMap::new();
    let apps = app_binaries(&store_path);

    let mut linked_names: HashSet<String> = HashSet::new();
    for src in find_binaries(&store_path)? {
        let name = src
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name != meta.package && !apps.contains(&name) {
            continue;
        }

        let dest = bin_dir.join(&name);
        let rel = match src.strip_prefix(&store_path) {
            Ok(r) => r.to_path_buf(),
            Err(_) => continue,
        };
        let target = crate::nspawn::container_path_for(&rel);
        match install_launcher(&dest, &target, kind, store) {
            Ok(()) => {
                wrapped.insert(name.clone(), dest.clone());
                linked.bin_links.push(dest.clone());
                manifest.push(('B', dest, src.clone()));
                linked_names.insert(name.clone());
            }
            Err(e) => eprintln!("univ: warning: not linking {name}: {e}"),
        }
    }

    let fallback_names: Vec<String> = apps.difference(&linked_names).cloned().collect();
    for name in fallback_names {
        if let Some(path) = find_executable_by_name(&store_path, &name) {
            let dest = bin_dir.join(&name);
            let rel = path
                .strip_prefix(&store_path)
                .unwrap_or(&path)
                .to_path_buf();
            let target = crate::nspawn::container_path_for(&rel);
            if let Err(e) = install_launcher(&dest, &target, kind, store) {
                eprintln!("univ: warning: not linking fallback {}: {}", name, e);
                continue;
            }
            wrapped.insert(name.clone(), dest.clone());
            linked.bin_links.push(dest.clone());
            manifest.push(('B', dest.clone(), path.clone()));
            linked_names.insert(name.clone());
        }
    }

    if !linked_names.contains(&meta.package) {
        if let Some(path) = find_executable_by_name(&store_path, &meta.package) {
            let dest = bin_dir.join(&meta.package);
            let rel = path
                .strip_prefix(&store_path)
                .unwrap_or(&path)
                .to_path_buf();
            let target = crate::nspawn::container_path_for(&rel);
            if let Err(e) = install_launcher(&dest, &target, kind, store) {
                eprintln!("univ: warning: not linking {}: {}", meta.package, e);
            } else {
                wrapped.insert(meta.package.clone(), dest.clone());
                linked.bin_links.push(dest.clone());
                manifest.push(('B', dest, path.clone()));
            }
        }
    }

    if let Ok(entries) = fs::read_dir(store_path.join("usr/share/applications")) {
        for entry in entries.flatten() {
            let path = entry.path();
            let file_name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !file_name.ends_with(".desktop") {
                continue;
            }
            let text = match fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("univ: warning: skipping {file_name}: {e}");
                    continue;
                }
            };
            let (rewritten, copies) =
                rewrite_desktop(&text, &store_path, &icons_dir, &pixmaps_dir, &wrapped);
            let dest = apps_dir.join(format!("univ-{}-{}", meta.package, file_name));
            match write_desktop(&dest, &rewritten) {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("univ: warning: cannot write {file_name}: {e}");
                    continue;
                }
            }
            linked.desktop_files.push(dest.clone());
            manifest.push(('D', dest, path));
            for c in copies {
                if let Err(e) = copy_icon_file(&c.source, &c.dest) {
                    eprintln!(
                        "univ: warning: cannot install icon {}: {e}",
                        c.source.display()
                    );
                    continue;
                }
                linked.icons.push(c.dest.clone());
                manifest.push(('I', c.dest, c.source));
            }
        }
    }

    if !linked.desktop_files.is_empty() {
        refresh_desktop_db(&apps_dir);
    }

    write_manifest(sp, &manifest)?;
    crate::nspawn::rebuild_tree(store)?;
    if !crate::nspawn::bootstrapped(kind) {
        eprintln!(
            "univ: warning: the {} container has no base OS yet; run `univ bootstrap` \
             (needs root) so installed programs can run inside it",
            kind.name()
        );
    }
    Ok(linked)
}

fn refresh_desktop_db(apps_dir: &Path) {
    for tool in ["update-desktop-database", "kbuildsycoca6", "kbuildsycoca5"] {
        let arg: &OsStr = if tool == "update-desktop-database" {
            apps_dir.as_os_str()
        } else {
            OsStr::new("--noincremental")
        };
        let Ok(output) = Command::new(tool).arg(arg).output() else {
            continue;
        };
        if !output.status.success() {
            eprintln!(
                "univ: warning: {tool}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }
}

pub fn unlink(package: &str) -> io::Result<usize> {
    let state = Store::state_dir()?;
    let (pkg, arch) = match package.rsplit_once(':') {
        Some((p, a)) => (p, Some(a)),
        None => (package, None),
    };
    let mut removed = 0;
    let mut found = false;
    if let Ok(entries) = fs::read_dir(&state) {
        for entry in entries.flatten() {
            let dir_name = entry.file_name().to_string_lossy().into_owned();
            let Some(sp) = StorePath::parse(&dir_name) else {
                continue;
            };
            let matches = sp.name() == pkg || sp.name().starts_with(&format!("{pkg}-"));
            if !matches {
                continue;
            }
            if let Some(arch) = arch {
                let meta_arch = crate::deb::read_meta(&sp)
                    .ok()
                    .map(|m| m.architecture)
                    .unwrap_or_default();
                if meta_arch != arch {
                    continue;
                }
            }
            found = true;
            removed += remove_artifacts(&sp)?;
            let _ = fs::remove_dir_all(entry.path());
        }
    }
    if !found {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no installed package matching '{package}'"),
        ));
    }
    Ok(removed)
}

fn required_by(installed: &[crate::resolve::Installed], name: &str) -> Vec<String> {
    installed
        .iter()
        .filter(|p| {
            p.meta
                .depends
                .iter()
                .flatten()
                .any(|d| d.package.split(':').next().unwrap_or("") == name)
        })
        .map(|p| p.meta.package.clone())
        .collect()
}

fn remove_orphans(store: &Store) -> Vec<String> {
    let mut removed = Vec::new();
    let mut lock = crate::lock::Lockfile::load();
    loop {
        let installed = crate::resolve::installed_packages(store);
        let orphan = installed.iter().find(|p| {
            crate::store::is_auto(&p.sp) && required_by(&installed, &p.meta.package).is_empty()
        });
        let Some(orphan) = orphan else {
            break;
        };
        let sp = orphan.sp.clone();
        let name = orphan.meta.package.clone();
        let arch = orphan.meta.architecture.clone();
        let _ = remove_artifacts(&sp);
        if let Ok(state) = Store::state_dir() {
            let _ = fs::remove_dir_all(state.join(sp.to_string()));
        }
        let _ = fs::remove_dir_all(store.base().join(sp.to_string()));
        lock.remove(&name, &arch);
        removed.push(name);
    }
    let _ = lock.save();
    let _ = crate::nspawn::rebuild_tree(store);
    removed
}

pub fn autoclean() -> io::Result<Vec<String>> {
    let store = Store::open()?;
    Ok(remove_orphans(&store))
}

pub fn uninstall(package: &str) -> io::Result<(usize, usize, Vec<String>)> {
    let (pkg, arch) = match package.rsplit_once(':') {
        Some((p, a)) => (p, Some(a)),
        None => (package, None),
    };

    let store = Store::open()?;
    let installed = crate::resolve::installed_packages(&store);

    let targets: Vec<StorePath> = store
        .paths()?
        .into_iter()
        .filter(|sp| {
            let matches = sp.name() == pkg || sp.name().starts_with(&format!("{pkg}-"));
            if !matches {
                return false;
            }
            match arch {
                Some(a) => crate::deb::read_meta(sp)
                    .ok()
                    .map(|m| m.architecture == a)
                    .unwrap_or(false),
                None => true,
            }
        })
        .collect();

    let linked = unlink(package).unwrap_or_default();

    let mut lock = crate::lock::Lockfile::load();
    let mut removed = 0;
    for sp in &targets {
        let still_needed: Vec<String> = installed
            .iter()
            .filter(|p| {
                p.sp != *sp
                    && p.meta
                        .depends
                        .iter()
                        .flatten()
                        .any(|d| d.package.split(':').next().unwrap_or("") == pkg)
            })
            .map(|p| p.meta.package.clone())
            .collect();
        if !still_needed.is_empty() {
            eprintln!(
                "univ: warning: {} is still required by {}",
                sp.name(),
                still_needed.join(", ")
            );
        }
        if let Some(p) = installed.iter().find(|p| p.sp == *sp) {
            lock.remove(&p.meta.package, &p.meta.architecture);
        }
        fs::remove_dir_all(store.base().join(sp.to_string()))?;
        removed += 1;
    }
    let _ = lock.save();

    let orphans = remove_orphans(&store);

    if linked == 0 && removed == 0 && orphans.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no installed package matching '{package}'"),
        ));
    }
    Ok((linked, removed, orphans))
}

pub fn remove_artifacts(sp: &StorePath) -> io::Result<usize> {
    let manifest = Store::state_dir()?.join(sp.to_string()).join("links");
    let mut removed = 0;
    if let Ok(text) = fs::read_to_string(&manifest) {
        for line in text.lines() {
            let mut it = line.split('\t');
            let _kind = it.next();
            let (Some(dest), Some(_src)) = (it.next(), it.next()) else {
                continue;
            };
            let dest = PathBuf::from(dest);
            if dest.exists() || fs::symlink_metadata(&dest).is_ok() {
                let _ = fs::remove_file(&dest);
                removed += 1;
            }
        }
    }
    Ok(removed)
}

fn write_desktop(dest: &Path, text: &str) -> io::Result<()> {
    let mut text = text.to_string();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    fs::write(dest, text.as_bytes())?;
    fs::set_permissions(dest, fs::Permissions::from_mode(0o755))
}

fn app_binaries(store_path: &Path) -> HashSet<String> {
    let mut apps = HashSet::new();
    if let Ok(entries) = fs::read_dir(store_path.join("usr/share/applications")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            for line in text.lines() {
                let Some((key, value)) = line.split_once('=') else {
                    continue;
                };
                if !matches!(key.trim_end(), "Exec" | "TryExec") {
                    continue;
                }
                if let Some(name) = value
                    .split_whitespace()
                    .next()
                    .and_then(|c| Path::new(c).file_name())
                {
                    apps.insert(name.to_string_lossy().into_owned());
                }
            }
        }
    }
    apps
}

pub fn find_binaries(store_path: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for rel in BIN_DIRS {
        let dir = store_path.join(rel);
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_file() && (meta.permissions().mode() & 0o111 != 0) {
                out.push(path);
            }
        }
    }
    let fallback_dirs = ["usr/lib", "opt", "usr/share"];
    for rel in &fallback_dirs {
        let base = store_path.join(rel);
        if let Ok(entries) = fs::read_dir(&base) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    out.push(path.clone());
                } else if path.is_dir() {
                    if let Ok(inner) = fs::read_dir(&path) {
                        for inner_entry in inner.flatten() {
                            let inner_path = inner_entry.path();
                            if inner_path.is_file() {
                                out.push(inner_path.clone());
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

fn install_launcher(
    dest: &Path,
    target: &str,
    kind: crate::nspawn::ContainerKind,
    store: &Store,
) -> io::Result<()> {
    let container_root = crate::nspawn::root(kind)?;
    let text = crate::nspawn::launcher(target, &container_root, store.base());
    match fs::symlink_metadata(dest) {
        Ok(md) if md.file_type().is_symlink() => {
            let _ = fs::remove_file(dest);
        }
        Ok(_) => {
            let existing = fs::read_to_string(dest).unwrap_or_default();
            if !existing.contains(crate::nspawn::LAUNCHER_MARKER) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "{} already exists and is not managed by univ",
                        dest.display()
                    ),
                ));
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    fs::write(dest, text.as_bytes())?;
    fs::set_permissions(dest, fs::Permissions::from_mode(0o755))
}

struct IconCopy {
    source: PathBuf,
    dest: PathBuf,
}

fn rewrite_desktop(
    text: &str,
    store_path: &Path,
    icons_dir: &Path,
    pixmaps_dir: &Path,
    wrapped: &HashMap<String, PathBuf>,
) -> (String, Vec<IconCopy>) {
    let mut copies = Vec::new();
    let rewritten = text
        .lines()
        .map(|line| {
            let Some((key, value)) = line.split_once('=') else {
                return line.to_string();
            };
            let key = key.trim_end();
            match key {
                "Exec" | "TryExec" => format!("{key}={}", rewrite_exec(value, wrapped)),
                "Icon" => {
                    let (value, mut cs) = rewrite_icon(value, store_path, icons_dir, pixmaps_dir);
                    copies.append(&mut cs);
                    format!("{key}={value}")
                }
                k if k == "Name" || k.starts_with("Name[") => {
                    format!("{k}={}", suffix_name(value))
                }
                _ => line.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    (rewritten, copies)
}

fn rewrite_exec(value: &str, wrapped: &HashMap<String, PathBuf>) -> String {
    let mut split = value.splitn(2, |c: char| c.is_whitespace());
    let cmd = split.next().unwrap_or("");
    let rest = split.next().unwrap_or("").trim_start();
    let base = Path::new(cmd)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| cmd.to_string());
    let new_cmd = match wrapped.get(&base) {
        Some(w) => w.to_string_lossy().into_owned(),
        None => cmd.to_string(),
    };
    if rest.is_empty() {
        new_cmd
    } else {
        format!("{new_cmd} {rest}")
    }
}

fn suffix_name(value: &str) -> String {
    if value.trim_end().ends_with(DESKTOP_SUFFIX) {
        value.to_string()
    } else {
        format!("{value}{DESKTOP_SUFFIX}")
    }
}

fn rewrite_icon(
    value: &str,
    store_path: &Path,
    icons_dir: &Path,
    pixmaps_dir: &Path,
) -> (String, Vec<IconCopy>) {
    let mut copies = Vec::new();
    let value = value.trim();
    if value.starts_with('/') {
        if let Some(rel) = value.strip_prefix("/usr/share/icons/") {
            let source = store_path.join("usr/share/icons").join(rel);
            if source.is_file() {
                let dest = icons_dir.join(rel);
                copies.push(IconCopy {
                    source,
                    dest: dest.clone(),
                });
                return (dest.to_string_lossy().into_owned(), copies);
            }
        }
        if let Some(rel) = value.strip_prefix("/usr/share/pixmaps/") {
            let source = store_path.join("usr/share/pixmaps").join(rel);
            if source.is_file() {
                let dest = pixmaps_dir.join(rel);
                copies.push(IconCopy {
                    source,
                    dest: dest.clone(),
                });
                return (dest.to_string_lossy().into_owned(), copies);
            }
        }
        return (value.to_string(), copies);
    }
    for copy in find_icon_files(store_path, value, icons_dir, pixmaps_dir) {
        copies.push(copy);
    }
    (value.to_string(), copies)
}

fn find_icon_files(
    store_path: &Path,
    name: &str,
    icons_dir: &Path,
    pixmaps_dir: &Path,
) -> Vec<IconCopy> {
    let mut out = Vec::new();
    let theme_root = store_path.join("usr/share/icons");
    collect_icon_dir(&theme_root, name, &theme_root, icons_dir, &mut out);

    let pix_root = store_path.join("usr/share/pixmaps");
    if let Ok(entries) = fs::read_dir(&pix_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            if stem == name {
                out.push(IconCopy {
                    dest: pixmaps_dir.join(path.file_name().unwrap_or_default()),
                    source: path,
                });
            }
        }
    }
    out
}

fn collect_icon_dir(
    dir: &Path,
    name: &str,
    root: &Path,
    icons_dir: &Path,
    out: &mut Vec<IconCopy>,
) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_icon_dir(&path, name, root, icons_dir, out);
            } else if path.is_file() {
                if let Some(stem) = path.file_stem().map(|s| s.to_string_lossy()) {
                    if stem == name {
                        let rel = path.strip_prefix(root).unwrap_or(&path);
                        out.push(IconCopy {
                            dest: icons_dir.join(rel),
                            source: path,
                        });
                    }
                }
            }
        }
    }
}

fn find_executable_by_name(store_path: &Path, name: &str) -> Option<PathBuf> {
    let search_dirs = ["usr/lib", "usr/share", "opt"];
    for dir in &search_dirs {
        let base = store_path.join(dir);
        if let Ok(entries) = fs::read_dir(&base) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
                        if file_name == name {
                            return Some(path);
                        }
                    }
                } else if path.is_dir() {
                    if let Ok(inner) = fs::read_dir(&path) {
                        for inner_entry in inner.flatten() {
                            let inner_path = inner_entry.path();
                            if inner_path.is_file() {
                                if let Some(inner_name) =
                                    inner_path.file_name().and_then(|n| n.to_str())
                                {
                                    if inner_name == name {
                                        return Some(inner_path);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

fn copy_icon_file(source: &Path, dest: &Path) -> io::Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(source, dest)?;
    Ok(())
}

fn write_manifest(sp: &StorePath, entries: &[(char, PathBuf, PathBuf)]) -> io::Result<()> {
    let dir = Store::state_dir()?.join(sp.to_string());
    fs::create_dir_all(&dir)?;
    let mut f = fs::File::create(dir.join("links"))?;
    writeln!(f, "# univ link manifest v1")?;
    for (kind, dest, source) in entries {
        writeln!(f, "{kind}\t{}\t{}", dest.display(), source.display())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deb::Dep;

    fn test_home(root: &Path, label: &str) -> PathBuf {
        let home = root.join(label);
        fs::create_dir_all(&home).unwrap();
        home
    }

    fn fake_store_path(store: &Store, sp: &StorePath) -> PathBuf {
        let root = store.base().join(sp.to_string());
        let bin = root.join("usr/bin/foo");
        fs::create_dir_all(bin.parent().unwrap()).unwrap();
        fs::write(&bin, "#!/bin/sh\necho hi\n").unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();

        let apps = root.join("usr/share/applications");
        fs::create_dir_all(&apps).unwrap();
        fs::write(
            apps.join("foo.desktop"),
            "[Desktop Entry]\nType=Application\nName=Foo Bar\n\
             Exec=/usr/bin/foo --new-window\n\
             Icon=/usr/share/icons/hicolor/scalable/apps/foo.svg\n",
        )
        .unwrap();

        let icon = root.join("usr/share/icons/hicolor/scalable/apps/foo.svg");
        fs::create_dir_all(icon.parent().unwrap()).unwrap();
        fs::write(&icon, "<svg/>").unwrap();
        root
    }

    fn sp() -> StorePath {
        StorePath::parse(&format!("{}-hello-1.0", "a".repeat(64))).unwrap()
    }

    fn meta() -> DebMeta {
        DebMeta {
            package: "hello".into(),
            version: "1.0".into(),
            architecture: "all".into(),
            ..Default::default()
        }
    }

    fn with_env<F: FnOnce(&Store, &Path) -> io::Result<()>>(label: &str, f: F) {
        let _g = crate::store::TEST_HOME_LOCK.lock().unwrap();
        let tmp =
            std::env::temp_dir().join(format!("univ-link-test-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let home = test_home(&tmp, "home");
        let store = crate::store::test_store(&home.join(".local/share/univ/store"));
        fs::create_dir_all(store.base()).unwrap();
        let old_home = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", &home) };
        let result = f(&store, &home);
        if let Some(old) = old_home {
            unsafe { std::env::set_var("HOME", old) };
        } else {
            unsafe { std::env::remove_var("HOME") };
        }
        fs::remove_dir_all(&tmp).unwrap();
        result.unwrap();
    }

    #[test]
    fn link_and_unlink_roundtrip() {
        with_env("roundtrip", |store, home| {
            let sp = sp();
            fake_store_path(store, &sp);
            let linked = link_package(store, &sp, &meta()).unwrap();

            assert_eq!(linked.bin_links, vec![home.join(".local/bin/foo")]);
            let launcher = home.join(".local/bin/foo");
            assert!(launcher.is_file(), "launcher must be a script");
            let script = fs::read_to_string(&launcher).unwrap();
            assert!(
                script.starts_with("#!/bin/sh") && script.contains(crate::nspawn::LAUNCHER_MARKER),
                "{script}"
            );
            assert!(
                script.contains("--rootless=true run --bundle \"$BUNDLE\""),
                "{script}"
            );
            assert!(script.contains("TARGET=\"/usr/bin/foo\""), "{script}");

            let container = crate::nspawn::root(crate::nspawn::ContainerKind::Deb).unwrap();
            let tree_link = container.join("usr/bin/foo");
            assert!(
                tree_link.is_symlink(),
                "container tree must merge the package binary"
            );
            assert_eq!(
                fs::read_link(&tree_link).unwrap(),
                Path::new("../../../store")
                    .join(sp.to_string())
                    .join("usr/bin/foo")
            );
            assert!(
                tree_link.exists(),
                "relative link must resolve to the store on the host"
            );

            let desktop = home.join(".local/share/applications/univ-hello-foo.desktop");
            let text = fs::read_to_string(&desktop).unwrap();
            assert!(text.ends_with('\n'), "desktop file must end with newline");
            use std::os::unix::fs::MetadataExt;
            assert_ne!(
                fs::metadata(&desktop).unwrap().mode() & 0o111,
                0,
                "desktop file must be executable for GNOME"
            );
            assert!(text.contains("Name=Foo Bar (univ)"), "{text}");
            assert!(
                text.contains(&format!("Exec={} --new-window", launcher.display())),
                "{text}"
            );
            let icon_dest = home.join(".local/share/icons/hicolor/scalable/apps/foo.svg");
            assert!(icon_dest.is_file());
            assert!(
                text.contains(&format!("Icon={}", icon_dest.display())),
                "{text}"
            );

            assert!(
                Store::state_dir()
                    .unwrap()
                    .join(sp.to_string())
                    .join("links")
                    .is_file()
            );

            let removed = unlink("hello").unwrap();
            assert!(removed >= 3, "removed {removed}");
            assert!(!launcher.exists() && fs::symlink_metadata(&launcher).is_err());
            assert!(!desktop.exists());
            assert!(!icon_dest.exists());
            assert!(!Store::state_dir().unwrap().join(sp.to_string()).exists());
            Ok(())
        });
    }

    #[test]
    fn helper_binaries_get_no_launcher() {
        with_env("helper-bin", |store, home| {
            let sp = sp();
            fake_store_path(store, &sp);
            let root = store.base().join(sp.to_string());
            let helper = root.join("usr/bin/foo-helper");
            fs::write(&helper, "#!/bin/sh\necho helper\n").unwrap();
            fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
            let linked = link_package(store, &sp, &meta()).unwrap();
            assert_eq!(linked.bin_links, vec![home.join(".local/bin/foo")]);
            assert!(!home.join(".local/bin/foo-helper").exists());
            Ok(())
        });
    }

    #[test]
    fn uninstall_removes_store_path() {
        with_env("uninstall", |_store, home| {
            let store = Store::init().unwrap();
            let sp = sp();
            fake_store_path(&store, &sp);
            crate::deb::write_meta(&meta(), &sp).unwrap();
            link_package(&store, &sp, &meta()).unwrap();

            let store_root = store.base().join(sp.to_string());
            assert!(store_root.is_dir());

            let (links, paths, orphans) = uninstall("hello").unwrap();
            assert!(links >= 3, "removed {links} links");
            assert_eq!(paths, 1);
            assert!(orphans.is_empty());
            assert!(!store_root.exists());
            assert!(!home.join(".local/bin/foo").exists());
            assert!(
                !crate::nspawn::root(crate::nspawn::ContainerKind::Deb)
                    .unwrap()
                    .join("usr/bin/foo")
                    .exists()
            );
            assert!(!Store::state_dir().unwrap().join(sp.to_string()).exists());
            Ok(())
        });
    }

    #[test]
    fn uninstall_removes_orphaned_dependencies() {
        with_env("uninstall-orphans", |_store, _home| {
            let store = Store::init().unwrap();

            let sp_app = StorePath::parse(&format!("{}-app", "a".repeat(64))).unwrap();
            let sp_lib = StorePath::parse(&format!("{}-lib", "b".repeat(64))).unwrap();
            let sp_core = StorePath::parse(&format!("{}-core", "c".repeat(64))).unwrap();
            let sp_manual = StorePath::parse(&format!("{}-manual", "d".repeat(64))).unwrap();

            let meta_app = DebMeta {
                package: "app".into(),
                version: "1.0".into(),
                architecture: "all".into(),
                depends: vec![vec![Dep::package_only("lib")]],
                ..Default::default()
            };
            let meta_lib = DebMeta {
                package: "lib".into(),
                version: "2.0".into(),
                architecture: "all".into(),
                depends: vec![vec![Dep::package_only("core")]],
                ..Default::default()
            };
            let meta_core = DebMeta {
                package: "core".into(),
                version: "3.0".into(),
                architecture: "all".into(),
                ..Default::default()
            };
            let meta_manual = DebMeta {
                package: "manual".into(),
                version: "1.0".into(),
                architecture: "all".into(),
                depends: vec![vec![Dep::package_only("core")]],
                ..Default::default()
            };

            for (sp, meta) in [
                (&sp_app, &meta_app),
                (&sp_lib, &meta_lib),
                (&sp_core, &meta_core),
                (&sp_manual, &meta_manual),
            ] {
                fs::create_dir_all(store.base().join(sp.to_string())).unwrap();
                crate::deb::write_meta(meta, sp).unwrap();
            }
            crate::store::mark_manual(&sp_app).unwrap();
            crate::store::mark_auto(&sp_lib).unwrap();
            crate::store::mark_auto(&sp_core).unwrap();
            crate::store::mark_manual(&sp_manual).unwrap();

            let (links, paths, orphans) = uninstall("app").unwrap();
            assert_eq!(links, 0);
            assert_eq!(paths, 1);
            assert_eq!(orphans, vec!["lib".to_string()]);
            assert!(!store.base().join(sp_app.to_string()).exists());
            assert!(!store.base().join(sp_lib.to_string()).exists());
            assert!(
                store.base().join(sp_core.to_string()).is_dir(),
                "core must stay: manual still depends on it"
            );
            assert!(store.base().join(sp_manual.to_string()).is_dir());
            Ok(())
        });
    }

    #[test]
    fn uninstall_arch_qualified() {
        with_env("uninstall-arch", |_store, _home| {
            let store = Store::init().unwrap();
            let sp32 = StorePath::parse(&format!("{}-hello-1.0-i386", "b".repeat(64))).unwrap();
            let sp64 = StorePath::parse(&format!("{}-hello-1.0-amd64", "c".repeat(64))).unwrap();
            for (sp, arch) in [(&sp32, "i386"), (&sp64, "amd64")] {
                fake_store_path(&store, sp);
                let mut m = meta();
                m.architecture = arch.into();
                crate::deb::write_meta(&m, sp).unwrap();
            }

            let (links, paths, orphans) = uninstall("hello:i386").unwrap();
            assert_eq!(links, 0);
            assert_eq!(paths, 1, "only the i386 store path is removed");
            assert!(orphans.is_empty());
            assert!(!store.base().join(sp32.to_string()).exists());
            assert!(
                store.base().join(sp64.to_string()).is_dir(),
                "amd64 instance must survive"
            );
            Ok(())
        });
    }

    #[test]
    fn does_not_clobber_foreign_files() {
        with_env("foreign", |store, home| {
            let sp = sp();
            fake_store_path(store, &sp);
            let bin_dir = home.join(".local/bin");
            fs::create_dir_all(&bin_dir).unwrap();
            fs::write(bin_dir.join("foo"), b"user file").unwrap();

            let linked = link_package(store, &sp, &meta()).unwrap();
            assert!(linked.bin_links.is_empty(), "must skip foreign file");
            assert_eq!(fs::read(bin_dir.join("foo")).unwrap(), b"user file");
            Ok(())
        });
    }
}
