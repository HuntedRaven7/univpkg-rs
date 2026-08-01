use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::deb::DebMeta;
use crate::resolve;
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
    let store_path = store.base().join(sp.to_string());
    let bin_dir = home.join(".local").join("bin");
    let apps_dir = home.join(".local").join("share").join("applications");
    let icons_dir = home.join(".local").join("share").join("icons");
    let pixmaps_dir = home.join(".local").join("share").join("pixmaps");

    fs::create_dir_all(&bin_dir)?;
    fs::create_dir_all(&apps_dir)?;

    let mut linked = Linked::default();
    let mut manifest: Vec<(char, PathBuf, PathBuf)> = Vec::new();
    let installed = resolve::installed_packages(store);
    let mut wrapped: HashMap<String, PathBuf> = HashMap::new();

    for src in find_binaries(&store_path)? {
        let name = src
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let dest = bin_dir.join(&name);
        let dirs = resolve::store_lib_dirs(&resolve::resolve_binary(&src, &installed));
        let result = if dirs.is_empty() {
            install_bin_link(&src, &dest, store.base())
        } else {
            match write_wrapper(&dest, &src, &dirs) {
                Ok(()) => {
                    wrapped.insert(name.clone(), dest.clone());
                    Ok(())
                }
                Err(e) => Err(e),
            }
        };
        match result {
            Ok(()) => {
                linked.bin_links.push(dest.clone());
                manifest.push(('B', dest, src));
            }
            Err(e) => eprintln!("univ: warning: not linking {name}: {e}"),
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
            let (rewritten, copies) = rewrite_desktop(
                &text,
                &store_path,
                &icons_dir,
                &pixmaps_dir,
                &wrapped,
            );
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
            p.meta.depends.iter().flatten().any(|d| {
                d.package.split(':').next().unwrap_or("") == name
            })
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
            crate::store::is_auto(&p.sp)
                && required_by(&installed, &p.meta.package).is_empty()
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
                    && p.meta.depends.iter().flatten().any(|d| {
                        d.package.split(':').next().unwrap_or("") == pkg
                    })
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

fn write_wrapper(dest: &Path, target: &Path, dirs: &[PathBuf]) -> io::Result<()> {
    let ld = dirs
        .iter()
        .map(|d| d.display().to_string())
        .collect::<Vec<_>>()
        .join(":");
    let script = format!(
        "#!/bin/sh\nLD_LIBRARY_PATH=\"{ld}${{LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}}\" exec \"{target}\" \"$@\"\n",
        target = target.display()
    );
    fs::write(dest, script.as_bytes())?;
    fs::set_permissions(dest, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

pub(crate) fn find_binaries(store_path: &Path) -> io::Result<Vec<PathBuf>> {
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
            if meta.is_file() && meta.permissions().mode() & 0o111 != 0 {
                out.push(path);
            }
        }
    }
    Ok(out)
}

fn install_bin_link(src: &Path, dest: &Path, store_base: &Path) -> io::Result<()> {
    match fs::symlink_metadata(dest) {
        Ok(md) => {
            if md.file_type().is_symlink() {
                let target = fs::read_link(dest)?;
                if !target.starts_with(store_base) {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("{} points outside the store", dest.display()),
                    ));
                }
                fs::remove_file(dest)?;
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} already exists", dest.display()),
                ));
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    symlink(src, dest)
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
                "Exec" | "TryExec" => {
                    format!("{key}={}", rewrite_exec(value, store_path, wrapped))
                }
                "Icon" => {
                    let (value, mut cs) =
                        rewrite_icon(value, store_path, icons_dir, pixmaps_dir);
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

fn rewrite_exec(
    value: &str,
    store_path: &Path,
    wrapped: &HashMap<String, PathBuf>,
) -> String {
    let mut split = value.splitn(2, |c: char| c.is_whitespace());
    let cmd = split.next().unwrap_or("");
    let rest = split.next().unwrap_or("").trim_start();
    let new_cmd = if cmd.starts_with('/') {
        match Path::new(cmd).strip_prefix("/usr/bin/") {
            Ok(rel) => {
                let name = rel
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                match wrapped.get(&name) {
                    Some(w) => w.to_string_lossy().into_owned(),
                    None => store_path
                        .join("usr/bin")
                        .join(name)
                        .to_string_lossy()
                        .into_owned(),
                }
            }
            Err(_) => store_path
                .join(cmd.trim_start_matches('/'))
                .to_string_lossy()
                .into_owned(),
        }
    } else {
        match wrapped.get(cmd) {
            Some(w) => w.to_string_lossy().into_owned(),
            None => {
                let in_store = store_path.join("usr/bin").join(cmd);
                if in_store.exists() {
                    in_store.to_string_lossy().into_owned()
                } else {
                    cmd.to_string()
                }
            }
        }
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
                copies.push(IconCopy { source, dest: dest.clone() });
                return (dest.to_string_lossy().into_owned(), copies);
            }
        }
        if let Some(rel) = value.strip_prefix("/usr/share/pixmaps/") {
            let source = store_path.join("usr/share/pixmaps").join(rel);
            if source.is_file() {
                let dest = pixmaps_dir.join(rel);
                copies.push(IconCopy { source, dest: dest.clone() });
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
    theme_root: &Path,
    icons_dir: &Path,
    out: &mut Vec<IconCopy>,
) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_icon_dir(&path, name, theme_root, icons_dir, out);
        } else {
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            if stem == name
                && let Ok(rel) = path.strip_prefix(theme_root) {
                    out.push(IconCopy {
                        dest: icons_dir.join(rel),
                        source: path,
                    });
                }
        }
    }
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
        let tmp = std::env::temp_dir().join(format!(
            "univ-link-test-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let store = crate::store::test_store(&tmp.join("store"));
        fs::create_dir_all(store.base()).unwrap();
        let home = test_home(&tmp, "home");
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
            let link = home.join(".local/bin/foo");
            assert!(link.is_symlink());
            assert_eq!(
                fs::read_link(&link).unwrap(),
                store.base().join(sp.to_string()).join("usr/bin/foo")
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
            let store_bin = store.base().join(sp.to_string()).join("usr/bin/foo");
            assert!(
                text.contains(&format!("Exec={} --new-window", store_bin.display())),
                "{text}"
            );
            let icon_dest = home.join(".local/share/icons/hicolor/scalable/apps/foo.svg");
            assert!(icon_dest.is_file());
            assert!(text.contains(&format!("Icon={}", icon_dest.display())), "{text}");

            assert!(Store::state_dir()
                .unwrap()
                .join(sp.to_string())
                .join("links")
                .is_file());

            let removed = unlink("hello").unwrap();
            assert!(removed >= 3, "removed {removed}");
            assert!(!link.exists() && fs::symlink_metadata(&link).is_err());
            assert!(!desktop.exists());
            assert!(!icon_dest.exists());
            assert!(!Store::state_dir().unwrap().join(sp.to_string()).exists());
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

            for (sp, meta) in [(&sp_app, &meta_app), (&sp_lib, &meta_lib), (&sp_core, &meta_core), (&sp_manual, &meta_manual)] {
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
