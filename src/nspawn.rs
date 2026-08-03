use std::collections::HashSet;
use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Component, Path, PathBuf};

use crate::store::{Store, StorePath};

pub const LAUNCHER_MARKER: &str = "# univ nspawn launcher v2";

const SKELETON_DIRS: &[&str] = &[
    "usr",
    "etc",
    "opt",
    "var",
    "run",
    "tmp",
    "dev",
    "proc",
    "sys",
];

const SKELETON_LINKS: &[(&str, &str)] = &[
    ("bin", "usr/bin"),
    ("sbin", "usr/sbin"),
    ("lib", "usr/lib"),
    ("lib64", "usr/lib64"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerKind {
    Deb,
    Rpm,
}

impl ContainerKind {
    pub fn name(self) -> &'static str {
        match self {
            ContainerKind::Deb => "deb",
            ContainerKind::Rpm => "rpm",
        }
    }
}

pub fn package_kind(sp: &crate::store::StorePath) -> ContainerKind {
    let meta = crate::store::Store::state_dir()
        .map(|dir| dir.join(sp.to_string()).join("meta"))
        .and_then(|path| fs::read_to_string(&path))
        .unwrap_or_default();
    if meta.lines().any(|l| l.trim() == "Kind: rpm") {
        ContainerKind::Rpm
    } else {
        ContainerKind::Deb
    }
}

pub fn root(kind: ContainerKind) -> io::Result<PathBuf> {
    Ok(crate::store::Store::root()?.join(kind.name()))
}

pub fn initialized() -> bool {
    [ContainerKind::Deb, ContainerKind::Rpm]
        .iter()
        .any(|k| root(*k).map(|r| r.join("usr").is_dir()).unwrap_or(false))
}

pub fn init(kind: ContainerKind) -> io::Result<()> {
    let root = root(kind)?;
    fs::create_dir_all(&root)?;
    for d in SKELETON_DIRS {
        fs::create_dir_all(root.join(d))?;
    }
    for (name, target) in SKELETON_LINKS {
        let link = root.join(name);
        match fs::symlink_metadata(&link) {
            Ok(md) if md.file_type().is_symlink() => {}
            Ok(_) => {
                let _ = fs::remove_file(&link);
                symlink(target, &link)?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => symlink(target, &link)?,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

pub fn map_dest_rel(rel: &Path) -> PathBuf {
    let mut comps = rel.components();
    let first = comps.next();
    match first {
        Some(Component::Normal(first)) => match first.to_str() {
            Some("bin") => PathBuf::from("usr/bin").join(comps.as_path()),
            Some("sbin") => PathBuf::from("usr/sbin").join(comps.as_path()),
            Some("lib") => PathBuf::from("usr/lib").join(comps.as_path()),
            Some("lib64") => PathBuf::from("usr/lib64").join(comps.as_path()),
            _ => rel.to_path_buf(),
        },
        _ => rel.to_path_buf(),
    }
}

pub fn container_path_for(package_rel: &Path) -> String {
    let dest = map_dest_rel(package_rel);
    format!("/{}", dest.display())
}

fn symlink_target(sp: &StorePath, dest_rel: &Path, src_rel: &Path) -> PathBuf {
    let up = dest_rel.components().count();
    let mut target = PathBuf::new();
    for _ in 0..up {
        target.push("..");
    }
    target.push("store");
    target.push(sp.to_string());
    target.push(src_rel);
    target
}

pub fn rebuild_tree(store: &Store) -> io::Result<usize> {
    init(ContainerKind::Deb)?;
    init(ContainerKind::Rpm)?;
    let mut added = 0;
    let mut wanted_deb: HashSet<PathBuf> = HashSet::new();
    let mut wanted_rpm: HashSet<PathBuf> = HashSet::new();
    for sp in store.paths()? {
        let base = store.base().join(sp.to_string());
        if !base.is_dir() {
            continue;
        }
        let kind = package_kind(&sp);
        let (root, wanted) = match kind {
            ContainerKind::Deb => (root(ContainerKind::Deb)?, &mut wanted_deb),
            ContainerKind::Rpm => (root(ContainerKind::Rpm)?, &mut wanted_rpm),
        };
        merge(&base, &sp, &root, &PathBuf::new(), &mut added, wanted)?;
    }
    remove_stale(&root(ContainerKind::Deb)?, &wanted_deb)?;
    remove_stale(&root(ContainerKind::Rpm)?, &wanted_rpm)?;
    Ok(added)
}

fn merge(
    base: &Path,
    sp: &StorePath,
    root: &Path,
    src_rel: &Path,
    added: &mut usize,
    wanted: &mut HashSet<PathBuf>,
) -> io::Result<()> {
    let entries = match fs::read_dir(base.join(src_rel)) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for entry in entries.flatten() {
        let child_src = src_rel.join(entry.file_name());
        let ft = match entry.file_type() {
            Ok(f) => f,
            Err(_) => continue,
        };
        let dest_rel = map_dest_rel(&child_src);
        let dest = root.join(&dest_rel);
        if ft.is_dir() {
            if !dest.is_dir() {
                if fs::symlink_metadata(&dest).is_ok() {
                    let _ = fs::remove_file(&dest);
                }
                fs::create_dir_all(&dest)?;
            }
            wanted.insert(dest_rel);
            merge(base, sp, root, &child_src, added, wanted)?;
        } else {
            let target = symlink_target(sp, &dest_rel, &child_src);
            wanted.insert(dest_rel);
            match fs::symlink_metadata(&dest) {
                Ok(md) if md.file_type().is_symlink() => {
                    if fs::read_link(&dest).ok() == Some(target.clone()) {
                        continue;
                    }
                    let _ = fs::remove_file(&dest);
                }
                Ok(_) => continue,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            symlink(&target, &dest)?;
            *added += 1;
        }
    }
    Ok(())
}

fn remove_stale(root: &Path, wanted: &HashSet<PathBuf>) -> io::Result<()> {
    let mut stale = Vec::new();
    collect_stale(root, root, wanted, &mut stale);
    for path in stale {
        let _ = fs::remove_file(&path);
    }
    Ok(())
}

fn collect_stale(root: &Path, dir: &Path, wanted: &HashSet<PathBuf>, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let ft = match entry.file_type() {
            Ok(f) => f,
            Err(_) => continue,
        };
        if ft.is_dir() {
            collect_stale(root, &path, wanted, out);
        } else if ft.is_symlink()
            && let Ok(target) = fs::read_link(&path)
            && target_points_at_store(&target)
            && let Ok(rel) = path.strip_prefix(root)
            && !wanted.contains(rel)
        {
            out.push(path);
        }
    }
}

fn target_points_at_store(target: &Path) -> bool {
    let mut seen_store = false;
    for comp in target.components() {
        match comp {
            Component::ParentDir => {}
            Component::Normal(n) if !seen_store && n != "store" => return false,
            Component::Normal(_) => seen_store = true,
            _ => return false,
        }
    }
    seen_store
}

pub fn on_filesystem(abs: &Path) -> bool {
    for kind in [ContainerKind::Deb, ContainerKind::Rpm] {
        if let Ok(root) = root(kind)
            && root.join("usr").is_dir()
            && let Ok(rel) = abs.strip_prefix("/")
            && root.join(rel).exists()
        {
            return true;
        }
    }
    abs.exists()
}

fn nspawn_bin() -> String {
    std::env::var("UNIV_NSPAWN").unwrap_or_else(|_| "systemd-nspawn".to_string())
}

fn shell_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

pub const BOOTSTRAP_MARKER: &str = ".univ-bootstrap";

fn os_release() -> String {
    fs::read_to_string("/etc/os-release").unwrap_or_default()
}

fn os_release_value(key: &str) -> Option<String> {
    for line in os_release().lines() {
        let (k, v) = line.split_once('=')?;
        if k == key {
            return Some(v.trim_matches('"').to_string());
        }
    }
    None
}

fn command_exists(tool: &str) -> bool {
    std::process::Command::new(tool)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

fn is_root() -> bool {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false)
}

fn run(cmd: &str, args: &[String]) -> io::Result<()> {
    run_with_env(cmd, args, &[])
}

fn run_with_env(cmd: &str, args: &[String], envs: &[(&str, &str)]) -> io::Result<()> {
    let mut argv: Vec<std::ffi::OsString> = Vec::new();
    for (k, v) in envs {
        argv.push(format!("{k}={v}").into());
    }
    argv.push(cmd.into());
    argv.extend(args.iter().map(std::ffi::OsString::from));

    let mut c = std::process::Command::new("env");
    if !is_root() {
        let mut sudo = std::process::Command::new("sudo");
        sudo.arg("env");
        c = sudo;
    }
    c.args(&argv);
    let status = c.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("`{cmd}` failed with {status}")))
    }
}

fn restore_ownership(root: &Path) -> io::Result<()> {
    if is_root() {
        return Ok(());
    }
    let uid = String::from_utf8_lossy(
        &std::process::Command::new("id").arg("-u").output()?.stdout,
    )
    .trim()
    .to_string();
    let gid = String::from_utf8_lossy(
        &std::process::Command::new("id").arg("-g").output()?.stdout,
    )
    .trim()
    .to_string();
    run(
        "chown",
        &["-R".to_string(), format!("{uid}:{gid}"), root.display().to_string()],
    )?;
    run("chmod", &["u+w".to_string(), root.display().to_string()])
}

pub fn bootstrapped(kind: ContainerKind) -> bool {
    root(kind)
        .map(|r| r.join(BOOTSTRAP_MARKER).is_file())
        .unwrap_or(false)
}

pub fn bootstrap() -> io::Result<()> {
    let mut errors: Vec<String> = Vec::new();
    for kind in [ContainerKind::Deb, ContainerKind::Rpm] {
        match bootstrap_kind(kind) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                eprintln!(
                    "univ: warning: skipping {} container bootstrap: {e}",
                    kind.name()
                );
            }
            Err(e) => errors.push(format!("{} container: {e}", kind.name())),
        }
    }
    if let Ok(store) = crate::store::Store::open() {
        let _ = rebuild_tree(&store);
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(io::Error::other(errors.join("\n")))
    }
}

fn bootstrap_kind(kind: ContainerKind) -> io::Result<()> {
    let root = root(kind)?;
    init(kind)?;
    if bootstrapped(kind) {
        return Ok(());
    }
    clear_symlinks(&root)?;
    match kind {
        ContainerKind::Rpm => rpm_bootstrap(&root)?,
        ContainerKind::Deb => deb_bootstrap(&root)?,
    }
    restore_ownership(&root)?;
    fs::write(root.join(BOOTSTRAP_MARKER), b"1\n")?;
    Ok(())
}

fn clear_symlinks(dir: &Path) -> io::Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let ft = match entry.file_type() {
            Ok(f) => f,
            Err(_) => continue,
        };
        if ft.is_symlink() {
            let _ = fs::remove_file(&path);
        } else if ft.is_dir() {
            clear_symlinks(&path)?;
        }
    }
    Ok(())
}

fn rpm_bootstrap(root: &Path) -> io::Result<()> {
    let dnf = match ["dnf", "dnf5", "microdnf"]
        .iter()
        .copied()
        .find(|t| command_exists(t))
    {
        Some(t) => t,
        None => {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "need dnf/microdnf to build the rpm container base OS",
            ));
        }
    };
    let reposdir = crate::store::Store::root()?.join("cache").join("bootstrap-repos");
    let _ = fs::remove_dir_all(&reposdir);
    fs::create_dir_all(&reposdir)?;
    let repos = crate::rpmrepo::repos()?;
    for repo in &repos {
        let file = reposdir.join(format!("univ-{}.repo", repo.name));
        fs::write(
            &file,
            format!(
                "[{}]\nname=univ {}\nbaseurl={}\nenabled=1\ngpgcheck=0\nskip_if_unavailable=1\n",
                repo.name, repo.name, repo.base
            ),
        )?;
    }
    let mut args = vec![format!("--installroot={}", root.display())];
    if let Some(rel) = os_release_value("VERSION_ID") {
        args.push(format!("--releasever={rel}"));
    }
    args.push(format!("--setopt=reposdir={}", reposdir.display()));
    args.extend([
        "--setopt=install_weak_deps=False".to_string(),
        "--setopt=tsflags=nodocs".to_string(),
        "install".to_string(),
    ]);
    args.extend(
        ["bash", "coreutils", "glibc", "filesystem", "dnf"]
            .into_iter()
            .map(String::from),
    );
    run(dnf, &args)
}

fn deb_bootstrap(root: &Path) -> io::Result<()> {
    let (debootstrap, share_dir) = fetch_debootstrap()?;
    let codename = if is_debian_like_host() {
        os_release_value("VERSION_CODENAME")
            .or_else(|| os_release_value("VERSION_ID"))
            .ok_or_else(|| {
                io::Error::other("no VERSION_CODENAME in /etc/os-release for debootstrap")
            })?
    } else {
        crate::repo::DIST.to_string()
    };
    let mirror = apt_mirror().unwrap_or_else(|_| {
        crate::repo::repos()
            .ok()
            .and_then(|rs| rs.first().map(|r| r.base.clone()))
            .unwrap_or_else(|| DEFAULT_DEB_MIRROR.to_string())
    });
    run_with_env(
        "sh",
        &[
            debootstrap,
            "--no-check-gpg".to_string(),
            "--variant=minbase".to_string(),
            format!("--arch={}", crate::repo::host_arch()),
            codename,
            root.display().to_string(),
            mirror,
        ],
        &[("DEBOOTSTRAP_DIR", &share_dir)],
    )
}

const DEFAULT_DEB_MIRROR: &str = "http://deb.debian.org/debian";

fn is_debian_like_host() -> bool {
    matches!(
        os_release_value("ID").unwrap_or_default().as_str(),
        "debian"
            | "ubuntu"
            | "linuxmint"
            | "elementary"
            | "pop"
            | "neon"
            | "raspbian"
            | "devuan"
    )
}

fn fetch_debootstrap() -> io::Result<(String, String)> {
    let cache = crate::store::Store::root()?.join("cache").join("debootstrap");
    let bin = cache.join("usr/sbin/debootstrap");
    if !bin.is_file() {
        let _ = fs::remove_dir_all(&cache);
        fs::create_dir_all(&cache)?;
        let bytes = crate::repo::fetch("debootstrap")?;
        crate::deb::extract_to(&bytes, &cache)?;
    }
    let share_dir = cache.join("usr/share/debootstrap");
    if !bin.is_file() || !share_dir.is_dir() {
        return Err(io::Error::other(
            "the debootstrap package did not contain the expected scripts",
        ));
    }
    Ok((
        bin.to_string_lossy().into_owned(),
        share_dir.to_string_lossy().into_owned(),
    ))
}

fn apt_mirror() -> io::Result<String> {
    let mut files = vec![PathBuf::from("/etc/apt/sources.list")];
    if let Ok(dir) = fs::read_dir("/etc/apt/sources.list.d") {
        for entry in dir.flatten() {
            files.push(entry.path());
        }
    }
    for file in files {
        if let Ok(text) = fs::read_to_string(&file) {
            for line in text.lines() {
                let line = line.split('#').next().unwrap_or("").trim();
                if let Some(rest) = line.strip_prefix("deb ") {
                    let uri = rest.split_whitespace().next().unwrap_or("");
                    if !uri.is_empty() {
                        return Ok(uri.to_string());
                    }
                }
            }
        }
    }
    Err(io::Error::other(
        "no deb mirror found in /etc/apt/sources.list(.d)",
    ))
}

pub fn launcher(target: &str, container_root: &Path, store_base: &Path) -> String {
    let c = shell_escape(&container_root.to_string_lossy());
    let s = shell_escape(&store_base.to_string_lossy());
    let t = shell_escape(target);
    format!(
        "#!/bin/sh\n\
         {LAUNCHER_MARKER}\n\
         CONTAINER=\"{c}\"\n\
         STORE=\"{s}\"\n\
         BINDS=\"\"\n\
         [ -d /tmp/.X11-unix ] && BINDS=\"$BINDS --bind=/tmp/.X11-unix:/tmp/.X11-unix\"\n\
         if [ -n \"${{XDG_RUNTIME_DIR:-}}\" ]; then\n\
         \t  BINDS=\"$BINDS --bind=${{XDG_RUNTIME_DIR}}:${{XDG_RUNTIME_DIR}}\"\n\
         fi\n\
         if [ -n \"${{HOME:-}}\" ]; then\n\
         \t  BINDS=\"$BINDS --bind=${{HOME}}:${{HOME}}\"\n\
         fi\n\
         exec {nspawn} --quiet --directory=\"$CONTAINER\" --bind-ro=\"$STORE:/store\" $BINDS --chdir=/ -- \"{t}\" \"$@\"\n",
        nspawn = nspawn_bin(),
    )
}

#[cfg(test)]
pub(crate) fn make_package(store: &Store, name: &str, files: &[(&str, &[u8])]) -> StorePath {
    store.add_tree(name, |dir, _ctx| {
        for (rel, data) in files {
            let p = dir.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, data).unwrap();
        }
        Ok(())
    })
    .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("univ-nspawn-{tag}-{}", std::process::id()))
    }

    fn with_home(tag: &str, f: impl FnOnce(std::path::PathBuf)) {
        let result = {
            let _g = crate::store::TEST_HOME_LOCK.lock().unwrap();
            let home = tmp_home(tag);
            let _ = fs::remove_dir_all(&home);
            fs::create_dir_all(&home).unwrap();
            let old = std::env::var_os("HOME");
            unsafe { std::env::set_var("HOME", &home) };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                f(home.clone());
            }));
            if let Some(old) = old {
                unsafe { std::env::set_var("HOME", old) };
            } else {
                unsafe { std::env::remove_var("HOME") };
            }
            let _ = fs::remove_dir_all(&home);
            drop(_g);
            result
        };
        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }

    #[test]
    fn skeleton_creates_fhs_aliases() {
        with_home("skeleton", |_home| {
            init(ContainerKind::Deb).unwrap();
            let root = root(ContainerKind::Deb).unwrap();
            for (name, target) in SKELETON_LINKS {
                assert_eq!(fs::read_link(root.join(name)).unwrap(), Path::new(target));
            }
            for d in SKELETON_DIRS {
                assert!(root.join(d).is_dir(), "missing skeleton dir {d}");
            }
        });
    }

    #[test]
    fn map_dest_rel_rewrites_top_level_aliases() {
        assert_eq!(map_dest_rel(Path::new("bin/foo")), Path::new("usr/bin/foo"));
        assert_eq!(map_dest_rel(Path::new("sbin/foo")), Path::new("usr/sbin/foo"));
        assert_eq!(map_dest_rel(Path::new("lib/libx.so")), Path::new("usr/lib/libx.so"));
        assert_eq!(map_dest_rel(Path::new("lib64/x.so")), Path::new("usr/lib64/x.so"));
        assert_eq!(
            map_dest_rel(Path::new("usr/share/app/foo")),
            Path::new("usr/share/app/foo")
        );
        assert_eq!(
            container_path_for(Path::new("bin/foo")),
            "/usr/bin/foo"
        );
    }

    #[test]
    fn rebuild_links_files_into_container_and_resolves_on_host() {
        with_home("rebuild", |home| {
            let store = crate::store::test_store(&home.join(".local/share/univ/store"));
            fs::create_dir_all(store.base()).unwrap();
            let sp = make_package(
                &store,
                "hello",
                &[
                    ("usr/bin/hello", b"#!/bin/sh\necho hi\n"),
                    ("usr/lib/x86_64-linux-gnu/libhello.so.1", b"ELF"),
                ],
            );

            let n = rebuild_tree(&store).unwrap();
            assert_eq!(n, 2);

            let root = root(ContainerKind::Deb).unwrap();
            let bin_link = root.join("usr/bin/hello");
            assert!(bin_link.is_symlink());
            assert_eq!(
                fs::read_link(&bin_link).unwrap(),
                Path::new("../../../store").join(sp.to_string()).join("usr/bin/hello")
            );
            assert!(
                bin_link.exists(),
                "relative link must resolve to the store file on the host"
            );
            assert_eq!(fs::read(&bin_link).unwrap(), b"#!/bin/sh\necho hi\n");

            let lib_link = root.join("usr/lib/x86_64-linux-gnu/libhello.so.1");
            assert!(lib_link.exists());
            assert_eq!(fs::read(&lib_link).unwrap(), b"ELF");
        });
    }

    #[test]
    fn rebuild_removes_stale_links_after_uninstall() {
        with_home("stale", |home| {
            let store = crate::store::test_store(&home.join(".local/share/univ/store"));
            fs::create_dir_all(store.base()).unwrap();
            let sp = make_package(&store, "hello", &[("usr/bin/hello", b"x")]);
            rebuild_tree(&store).unwrap();
            let root = root(ContainerKind::Deb).unwrap();
            assert!(root.join("usr/bin/hello").exists());

            fs::remove_dir_all(store.base().join(sp.to_string())).unwrap();
            rebuild_tree(&store).unwrap();
            assert!(!root.join("usr/bin/hello").exists());
            assert!(root.join("bin").is_symlink(), "skeleton must survive cleanup");
        });
    }

    #[test]
    fn launcher_script_runs_via_nspawn_with_store_bind() {
        with_home("launcher", |home| {
            let root = root(ContainerKind::Deb).unwrap();
            let store_base = home.join(".local/share/univ/store");
            let script = launcher("/usr/bin/hello", &root, &store_base);
            assert!(script.starts_with("#!/bin/sh"));
            assert!(script.contains(LAUNCHER_MARKER));
            assert!(script.contains("systemd-nspawn --quiet"));
            assert!(script.contains("--bind-ro=\"$STORE:/store\""));
            assert!(script.contains(&format!("STORE=\"{}\"", store_base.display())));
            assert!(script.contains("-- \"/usr/bin/hello\""));
            assert!(script.contains("--directory=\"$CONTAINER\""));
            assert!(script.contains(&format!("CONTAINER=\"{}\"", root.display())));
        });
    }

    #[test]
    fn on_filesystem_prefers_container_when_initialized() {
        with_home("onfs", |home| {
            init(ContainerKind::Deb).unwrap();
            let _ = home;
            let root = root(ContainerKind::Deb).unwrap();
            assert!(
                on_filesystem(Path::new("/bin/sh")) == root.join("bin/sh").exists()
                    || Path::new("/bin/sh").exists()
            );
        });
    }
}
