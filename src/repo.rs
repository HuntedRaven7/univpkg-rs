use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;

use crate::deb::{self, DebMeta, Dep};
use crate::resolve;
use crate::store::{sha256_hex, Store, StorePath};
use crate::version;

const COMPONENTS: &[&str] = &["main", "contrib", "non-free", "non-free-firmware"];

const DEFAULT_REPO_NAME: &str = "debian";
const DEFAULT_REPO_BASE: &str = "http://deb.debian.org/debian";
const DIST: &str = "stable";

const DEFAULT_ARCHES: &[&str] = &["amd64", "i386"];

pub fn host_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "x86" => "i386",
        "aarch64" => "arm64",
        other => other,
    }
}

const SYSTEM_PKGS: &[&str] = &[
    "libc6",
    "libgcc-s1",
    "libgcc1",
    "libstdc++6",
    "zlib1g",
    "liblzma5",
    "libzstd1",
    "libbz2-1.0",
    "liblz4-1",
    "libffi8",
    "libffi7",
    "libselinux1",
    "libpcre2-8-0",
    "libpcre3",
    "libpam0g",
    "libcap2",
    "libaudit1",
    "libmount1",
    "libblkid1",
    "libuuid1",
    "libsystemd0",
    "libexpat1",
    "libx11-6",
    "libx11-data",
    "libxext6",
    "libxcb1",
    "libxau6",
    "libxdmcp6",
    "libxinerama1",
    "libxrandr2",
    "libxrender1",
    "libxss1",
    "libxt6",
    "libsm6",
    "libice6",
    "libxmu6",
    "libxpm4",
    "libxft2",
    "libxi6",
    "libxcursor1",
    "libfontconfig1",
    "libfreetype6",
    "libpng16-16",
    "libjpeg62-turbo",
    "libxkbcommon0",
    "libglib2.0-0",
    "libglib2.0-0t64",
    "libcairo2",
    "libpango-1.0-0",
    "libpangoft2-1.0-0",
    "libpangocairo-1.0-0",
    "libatk1.0-0",
    "libatk1.0-0t64",
    "libgdk-pixbuf-2.0-0",
    "libgtk-3-0",
    "libgtk-3-0t64",
    "libwayland-client0",
    "libwayland-cursor0",
    "libwayland-egl1",
    "libx11-xcb1",
    "libxfixes3",
    "libxdamage1",
    "libxcomposite1",
    "libnss3",
    "libnspr4",
    "libdbus-1-3",
    "libutempter0",
];

#[derive(Clone, Debug)]
pub struct Repo {
    pub name: String,
    pub base: String,
    pub arches: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Package {
    pub package: String,
    pub version: String,
    pub architecture: String,
    pub multi_arch: Option<String>,
    pub depends: Vec<Vec<Dep>>,
    pub filename: String,
    pub sha256: Option<String>,
    pub description: String,
    pub provides: Vec<String>,
}

#[derive(Clone, Debug, Default)]
struct Index {
    by_name: HashMap<String, Vec<Package>>,
}

impl Index {
    fn insert(&mut self, pkg: Package) {
        self.by_name
            .entry(pkg.package.clone())
            .or_default()
            .push(pkg);
    }

    fn merge(&mut self, other: Index) {
        for (name, mut pkgs) in other.by_name {
            self.by_name.entry(name).or_default().append(&mut pkgs);
        }
    }

    fn candidates(&self, name: &str, arch: &str) -> Vec<Package> {
        self.by_name
            .get(name)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|p| {
                p.architecture == "all"
                    || p.architecture == arch
                    || p.multi_arch.as_deref() == Some("foreign")
            })
            .collect()
    }
}

pub fn repos() -> io::Result<Vec<Repo>> {
    let conf = Store::root()?.join("debrepos.conf");
    let mut out = Vec::new();
    if let Ok(text) = fs::read_to_string(&conf) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut it = line.split_whitespace();
            let (Some(name), Some(base)) = (it.next(), it.next()) else {
                continue;
            };
            let arches: Vec<String> = it.map(str::to_string).collect();
            out.push(Repo {
                name: name.to_string(),
                base: base.to_string(),
                arches: if arches.is_empty() {
                    default_arches()
                } else {
                    arches
                },
            });
        }
    }
    if out.is_empty() {
        out.push(Repo {
            name: DEFAULT_REPO_NAME.to_string(),
            base: DEFAULT_REPO_BASE.to_string(),
            arches: default_arches(),
        });
    }
    Ok(out)
}

pub fn add_repo(name: &str, base: &str, arches: &[String]) -> io::Result<()> {
    let conf = Store::root()?.join("debrepos.conf");
    if let Some(parent) = conf.parent() {
        fs::create_dir_all(parent)?;
    }
    let arches_part = if arches.is_empty() {
        default_arches().join(" ")
    } else {
        arches.join(" ")
    };
    let line = format!("{name} {base} {arches_part}\n");
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&conf)?
        .write_all(line.as_bytes())?;
    Ok(())
}

fn default_arches() -> Vec<String> {
    DEFAULT_ARCHES.iter().map(|a| a.to_string()).collect()
}

fn cache_path(repo: &Repo, arch: &str) -> io::Result<PathBuf> {
    Ok(Store::root()?
        .join("cache")
        .join(format!("{}.Packages.{arch}", repo.name)))
}

pub fn update(repo: &Repo) -> io::Result<usize> {
    let mut total = 0;
    for arch in &repo.arches {
        let mut index = Index::default();
        let mut fetched = 0;
        for component in COMPONENTS {
            let base = format!(
                "{}/dists/{}/{}/binary-{}/Packages",
                repo.base, DIST, component, arch
            );
            let label = format!("{} ({component}/{arch} Packages)", repo.name);
            match fetch_index(&base, &label) {
                Ok((text, _)) => {
                    let sub = parse_index(&text);
                    fetched += 1;
                    total += sub.by_name.values().map(|v| v.len()).sum::<usize>();
                    index.merge(sub);
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        if fetched == 0 {
            continue;
        }
        let path = cache_path(repo, arch)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, render_index(&index))?;
        let n = index.by_name.values().map(|v| v.len()).sum::<usize>();
        println!(
            "{} ({arch}): {} packages across {} components",
            crate::term::cyan(&repo.name),
            n,
            fetched
        );
    }
    Ok(total)
}

fn fetch_index(base: &str, label: &str) -> io::Result<(String, String)> {
    let mut last_err: Option<io::Error> = None;
    for ext in ["xz", "gz", ""] {
        match fetch_index_ext(base, ext, label) {
            Ok(v) => return Ok(v),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err
        .unwrap_or_else(|| io::Error::other("no Packages index available")))
}

fn fetch_index_ext(base: &str, ext: &str, label: &str) -> io::Result<(String, String)> {
    let url = if ext.is_empty() {
        base.to_string()
    } else {
        format!("{base}.{ext}")
    };
    let bytes = crate::term::http_get(&url, label, None)?;
    let text = match ext {
        "xz" => {
            let mut out = Vec::new();
            lzma_rs::xz_decompress(&mut &bytes[..], &mut out)
                .map_err(|e| io::Error::other(format!("xz: {e}")))?;
            String::from_utf8(out).map_err(io::Error::other)?
        }
        "gz" => {
            use std::io::Cursor;
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(Cursor::new(&bytes))
                .read_to_end(&mut out)?;
            String::from_utf8(out).map_err(io::Error::other)?
        }
        _ => String::from_utf8(bytes).map_err(io::Error::other)?,
    };
    Ok((text, url))
}

fn parse_index(text: &str) -> Index {
    let mut index = Index::default();
    let mut stanza: Vec<&str> = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            if let Some(pkg) = parse_stanza(&stanza) {
                index.insert(pkg);
            }
            stanza.clear();
        } else {
            stanza.push(line);
        }
    }
    if let Some(pkg) = parse_stanza(&stanza) {
        index.insert(pkg);
    }
    index
}

fn parse_stanza(lines: &[&str]) -> Option<Package> {
    let mut map: HashMap<&str, String> = HashMap::new();
    let mut cur: Option<&str> = None;
    for line in lines {
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(k) = cur
                && let Some(v) = map.get_mut(k) {
                    v.push('\n');
                    v.push_str(line.trim_start());
                }
        } else if let Some((k, v)) = line.split_once(':') {
            let k = k.trim();
            map.insert(k, v.trim().to_string());
            cur = Some(k);
        }
    }
    let package = map.get("Package")?;
    let version = map.get("Version")?;
    let filename = map.get("Filename").cloned()?;
    let mut depends = Vec::new();
    if let Some(d) = map.get("Depends") {
        depends.extend(deb::parse_depends(d));
    }
    if let Some(d) = map.get("Pre-Depends") {
        depends.extend(deb::parse_depends(d));
    }
    Some(Package {
        package: package.clone(),
        version: version.clone(),
        architecture: map.get("Architecture").cloned().unwrap_or_default(),
        multi_arch: map.get("Multi-Arch").cloned(),
        depends,
        filename,
        sha256: map.get("SHA256").cloned(),
        description: map
            .get("Description")
            .map(|d| d.lines().next().unwrap_or("").to_string())
            .unwrap_or_default(),
        provides: parse_provides(map.get("Provides").map(String::as_str).unwrap_or_default()),
    })
}

fn parse_provides(field: &str) -> Vec<String> {
    let mut out = Vec::new();
    for item in field.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let name = item
            .split_whitespace()
            .next()
            .unwrap_or(item)
            .split_once(':')
            .map(|(n, _)| n)
            .unwrap_or(item);
        if !out.iter().any(|v| v == name) {
            out.push(name.to_string());
        }
    }
    out
}

fn render_index(index: &Index) -> String {
    let mut all: Vec<&Package> = Vec::new();
    for group in index.by_name.values() {
        all.extend(group);
    }
    all.sort_by(|a, b| a.package.cmp(&b.package).then(a.version.cmp(&b.version)));
    let mut out = String::new();
    for pkg in all {
        out.push_str(&format!("Package: {}\n", pkg.package));
        out.push_str(&format!("Version: {}\n", pkg.version));
        if !pkg.architecture.is_empty() {
            out.push_str(&format!("Architecture: {}\n", pkg.architecture));
        }
        if let Some(ma) = &pkg.multi_arch {
            out.push_str(&format!("Multi-Arch: {ma}\n"));
        }
        if !pkg.description.is_empty() {
            out.push_str(&format!("Description: {}\n", pkg.description));
        }
        if !pkg.provides.is_empty() {
            out.push_str(&format!("Provides: {}\n", pkg.provides.join(", ")));
        }
        if !pkg.depends.is_empty() {
            out.push_str(&format!("Depends: {}\n", deb::render_depends(&pkg.depends)));
        }
        out.push_str(&format!("Filename: {}\n", pkg.filename));
        if let Some(h) = &pkg.sha256 {
            out.push_str(&format!("SHA256: {h}\n"));
        }
        out.push('\n');
    }
    out
}

fn read_index(repo: &Repo) -> io::Result<Index> {
    let mut index = Index::default();
    let mut missing = 0;
    for arch in &repo.arches {
        let path = cache_path(repo, arch)?;
        match fs::read_to_string(&path) {
            Ok(text) => index.merge(parse_index(&text)),
            Err(_) => missing += 1,
        }
    }
    if missing == repo.arches.len() {
        return Err(io::Error::other(format!(
            "no index for repo '{}'; run `univ update`",
            repo.name
        )));
    }
    Ok(index)
}

pub fn search(repo: &Repo, query: &str) -> Vec<Package> {
    let Ok(index) = read_index(repo) else {
        return Vec::new();
    };
    let q = query.to_lowercase();
    let mut out: Vec<Package> = index
        .by_name
        .values()
        .flatten()
        .filter(|p| {
            p.package.to_lowercase().contains(&q) || p.description.to_lowercase().contains(&q)
        })
        .cloned()
        .collect();
    out.sort_by(|a, b| a.package.cmp(&b.package).then(a.architecture.cmp(&b.architecture)));
    out
}

pub fn install(store: &Store, repo: &Repo, name: &str) -> io::Result<Vec<(StorePath, DebMeta)>> {
    let index = read_index(repo)
        .map_err(|_| io::Error::other(format!("no index for repo '{}'; run `univ update`", repo.name)))?;
    let installed = resolve::installed_packages(store);

    let (pkg_name, pkg_arch) = split_name_arch(name);

    let mut plan: Vec<Package> = Vec::new();
    let mut planned: HashSet<(String, String)> = HashSet::new();
    plan_package(
        &installed,
        &index,
        &pkg_name,
        pkg_arch.as_deref(),
        None,
        &mut plan,
        &mut planned,
    )?;

    let mut out = Vec::new();
    for (i, pkg) in plan.iter().enumerate() {
        let label = format!(
            "downloading {}-{} [{}]",
            pkg.package, pkg.version, pkg.architecture
        );
        let url = format!("{}/{}", repo.base, pkg.filename);
        let bytes = crate::term::http_get(&url, &label, None)?;
        if let Some(expected) = &pkg.sha256 {
            let got = sha256_hex(&bytes);
            if !got.eq_ignore_ascii_case(expected) {
                return Err(io::Error::other(format!(
                    "checksum mismatch for {}: expected {expected}, got {got}",
                    pkg.filename
                )));
            }
        }
        let (sp, meta) = deb::install(store, &bytes)?;
        deb::write_meta(&meta, &sp)?;
        if i + 1 == plan.len() {
            crate::store::mark_manual(&sp)?;
        } else {
            crate::store::mark_auto(&sp)?;
        }
        out.push((sp, meta));
    }
    if out.is_empty()
        && let Some(p) = installed
            .iter()
            .find(|p| p.meta.package == pkg_name)
    {
        crate::store::mark_manual(&p.sp)?;
    }
    Ok(out)
}

fn split_name_arch(s: &str) -> (String, Option<String>) {
    match s.rsplit_once(':') {
        Some((name, arch)) => (name.to_string(), Some(arch.to_string())),
        None => (s.to_string(), None),
    }
}

fn plan_package(
    installed: &[resolve::Installed],
    index: &Index,
    name: &str,
    arch: Option<&str>,
    constraint: Option<&(String, String)>,
    plan: &mut Vec<Package>,
    planned: &mut HashSet<(String, String)>,
) -> io::Result<()> {
    let desired = arch.map(str::to_owned).unwrap_or_else(|| host_arch().to_owned());

    if SYSTEM_PKGS.contains(&name) && desired == host_arch() {
        planned.insert((name.to_string(), desired.to_string()));
        return Ok(());
    }

    if let Some(p) = installed.iter().find(|p| {
        p.meta.package == name
            && (p.meta.architecture == desired || p.meta.architecture == "all")
            && constraint_ok(&p.meta.version, constraint)
    }) {
        planned.insert((p.meta.package.clone(), p.meta.architecture.clone()));
        return Ok(());
    }

    let candidate = best_candidate(index, name, &desired, constraint)
        .or_else(|| best_provider(index, name, &desired, constraint))
        .ok_or_else(|| {
            io::Error::other(format!("cannot satisfy dependency '{}'", name))
        })?;

    let key = (candidate.package.clone(), candidate.architecture.clone());
    if !planned.insert(key) {
        return Ok(());
    }

    for group in &candidate.depends {
        let mut chosen: Option<io::Error> = None;
        for alt in group {
            let (alt_name, alt_arch) = split_name_arch(&alt.package);
            let dep_arch: String = alt_arch.unwrap_or_else(|| {
                if candidate.architecture == "all" {
                    host_arch().to_string()
                } else {
                    candidate.architecture.clone()
                }
            });
            let mut sub_plan = Vec::new();
            let mut sub_planned = planned.clone();
            match plan_package(
                installed,
                index,
                &alt_name,
                Some(&dep_arch),
                alt.version.as_ref(),
                &mut sub_plan,
                &mut sub_planned,
            ) {
                Ok(()) => {
                    plan.extend(sub_plan);
                    planned.extend(sub_planned);
                    chosen = None;
                    break;
                }
                Err(e) => chosen = Some(e),
            }
        }
        if let Some(e) = chosen {
            return Err(e);
        }
    }

    plan.push(candidate);
    Ok(())
}

fn best_candidate(
    index: &Index,
    name: &str,
    arch: &str,
    constraint: Option<&(String, String)>,
) -> Option<Package> {
    index
        .candidates(name, arch)
        .into_iter()
        .filter(|p| constraint_ok(&p.version, constraint))
        .max_by(|a, b| {
            version::compare(&a.version, &b.version)
                .then(exact_arch(a, arch).cmp(&exact_arch(b, arch)))
        })
}

fn best_provider(
    index: &Index,
    name: &str,
    arch: &str,
    constraint: Option<&(String, String)>,
) -> Option<Package> {
    let mut providers: Vec<Package> = index
        .by_name
        .values()
        .flatten()
        .filter(|p| p.provides.iter().any(|v| v == name))
        .filter(|p| {
            p.architecture == "all"
                || p.architecture == arch
                || p.multi_arch.as_deref() == Some("foreign")
        })
        .filter(|p| constraint_ok(&p.version, constraint))
        .cloned()
        .collect();
    providers.sort_by(|a, b| {
        version::compare(&a.version, &b.version)
            .then(exact_arch(b, arch).cmp(&exact_arch(a, arch)))
            .then(a.package.cmp(&b.package))
    });
    providers.pop()
}

fn exact_arch(p: &Package, arch: &str) -> u8 {
    if p.architecture == arch || p.architecture == "all" {
        1
    } else {
        0
    }
}

fn constraint_ok(version: &str, c: Option<&(String, String)>) -> bool {
    match c {
        None => true,
        Some((op, req)) => version::satisfies(version, op, req),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg(name: &str, version: &str, depends: Vec<Vec<Dep>>) -> Package {
        pkg_arch(name, version, "all", depends)
    }

    fn pkg_arch(
        name: &str,
        version: &str,
        architecture: &str,
        depends: Vec<Vec<Dep>>,
    ) -> Package {
        Package {
            package: name.into(),
            version: version.into(),
            architecture: architecture.into(),
            multi_arch: None,
            depends,
            filename: format!("pool/{name}_{version}.deb"),
            sha256: None,
            description: String::new(),
            provides: Vec::new(),
        }
    }

    fn index_of(pkgs: Vec<Package>) -> Index {
        let mut idx = Index::default();
        for p in pkgs {
            idx.insert(p);
        }
        idx
    }

    fn plan(idx: &Index, name: &str, arch: Option<&str>) -> Vec<String> {
        let mut plan = Vec::new();
        let mut planned = HashSet::new();
        plan_package(&[], idx, name, arch, None, &mut plan, &mut planned).unwrap();
        plan.iter().map(|p| p.package.clone()).collect()
    }

    #[test]
    fn picks_highest_satisfying_version() {
        let idx = index_of(vec![
            pkg("foo", "1.0", vec![]),
            pkg("foo", "1.5", vec![]),
            pkg("foo", "2.0", vec![]),
        ]);
        assert_eq!(best_candidate(&idx, "foo", "amd64", None).unwrap().version, "2.0");
        let c = ("<<".to_string(), "2.0".to_string());
        assert_eq!(
            best_candidate(&idx, "foo", "amd64", Some(&c)).unwrap().version,
            "1.5"
        );
    }

    #[test]
    fn plans_transitive_closure() {
        let idx = index_of(vec![
            pkg(
                "app",
                "1.0",
                vec![
                    vec![Dep::package_only("liba")],
                    vec![Dep::package_only("libb")],
                ],
            ),
            pkg("liba", "2.0", vec![vec![Dep::package_only("libc")]]),
            pkg("libb", "3.0", vec![]),
            pkg("libc", "4.0", vec![]),
        ]);
        let names = plan(&idx, "app", None);
        assert_eq!(names, vec!["libc", "liba", "libb", "app"]);
    }

    #[test]
    fn system_packages_are_skipped() {
        let idx = index_of(vec![pkg("libc6", "2.40", vec![])]);
        assert!(plan(&idx, "libc6", None).is_empty());
    }

    #[test]
    fn unsatisfiable_dependency_errors() {
        let idx = index_of(vec![pkg("app", "1.0", vec![vec![Dep::package_only("ghost")]])]);
        let mut plan = Vec::new();
        let mut planned = HashSet::new();
        let err = plan_package(&[], &idx, "app", None, None, &mut plan, &mut planned);
        assert!(err.is_err());
    }

    #[test]
    fn alternative_group_falls_back() {
        let idx = index_of(vec![
            pkg("app", "1.0", vec![vec![Dep::package_only("liba"), Dep::package_only("libb")]]),
            pkg("libb", "3.0", vec![]),
        ]);
        assert_eq!(plan(&idx, "app", None), vec!["libb", "app"]);
    }

    #[test]
    fn i386_instance_of_system_package_is_installed() {
        let idx = index_of(vec![
            pkg_arch("libc6", "2.40", "amd64", vec![]),
            pkg_arch("libc6", "2.40", "i386", vec![]),
            pkg("app", "1.0", vec![vec![Dep { package: "libc6:i386".into(), version: None }]]),
        ]);
        let names = plan(&idx, "app", None);
        assert_eq!(names, vec!["libc6", "app"]);
        let mut planned = HashSet::new();
        let mut out = Vec::new();
        plan_package(&[], &idx, "app", None, None, &mut out, &mut planned).unwrap();
        let libc6 = out.iter().find(|p| p.package == "libc6").unwrap();
        assert_eq!(libc6.architecture, "i386");
    }

    #[test]
    fn unqualified_dep_stays_in_dependents_arch() {
        let idx = index_of(vec![
            pkg_arch("libgl1", "1.7", "amd64", vec![]),
            pkg_arch("libgl1", "1.7", "i386", vec![]),
            pkg_arch("app", "1.0", "i386", vec![vec![Dep::package_only("libgl1")]]),
        ]);
        let mut planned = HashSet::new();
        let mut out = Vec::new();
        plan_package(&[], &idx, "app", Some("i386"), None, &mut out, &mut planned).unwrap();
        let libgl1 = out.iter().find(|p| p.package == "libgl1").unwrap();
        assert_eq!(libgl1.architecture, "i386");
    }

    #[test]
    fn multi_arch_foreign_satisfies_cross_arch() {
        let mut tool = pkg_arch("babeltools", "2.0", "amd64", vec![]);
        tool.multi_arch = Some("foreign".into());
        let idx = index_of(vec![
            tool,
            pkg_arch("app", "1.0", "i386", vec![vec![Dep::package_only("babeltools")]]),
        ]);
        let names = plan(&idx, "app", Some("i386"));
        assert_eq!(names, vec!["babeltools", "app"]);
    }

    #[test]
    fn virtual_package_provides_satisfies_dependency() {
        let mut provider = pkg_arch("dbus-user-session", "1.0", "all", vec![]);
        provider.provides = vec!["default-dbus-session-bus".into()];
        let idx = index_of(vec![
            provider,
            pkg(
                "app",
                "1.0",
                vec![vec![Dep::package_only("default-dbus-session-bus")]],
            ),
        ]);
        let names = plan(&idx, "app", None);
        assert_eq!(names, vec!["dbus-user-session", "app"]);
    }

    #[test]
    fn real_package_preferred_over_provider() {
        let mut provider = pkg_arch("real-a", "1.0", "all", vec![]);
        provider.provides = vec!["virt".into()];
        let idx = index_of(vec![
            provider,
            pkg("virt", "1.0", vec![]),
            pkg("app", "1.0", vec![vec![Dep::package_only("virt")]]),
        ]);
        let names = plan(&idx, "app", None);
        assert_eq!(names, vec!["virt", "app"]);
    }

    #[test]
    fn search_matches_name_and_description() {
        let _g = crate::store::TEST_HOME_LOCK.lock().unwrap();

        let tmp = std::env::temp_dir().join(format!(
            "univ-search-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let old_home = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", &tmp) };

        let repo = Repo {
            name: "debian".into(),
            base: "http://example.invalid/debian".into(),
            arches: vec!["amd64".into()],
        };
        let cache_dir = tmp.join(".local/univ/cache");
        fs::create_dir_all(&cache_dir).unwrap();
        fs::write(
            cache_dir.join("debian.Packages.amd64"),
            "\
Package: steam-installer
Version: 1.0.0.81
Architecture: amd64
Filename: pool/non-free/s/steam/steam-installer_1.0.0.81_amd64.deb
Description: Steam launcher for Debian

Package: steam-libs-i386
Version: 1.0.0.81
Architecture: all
Depends: libc6
Filename: pool/non-free/s/steam/steam-libs-i386_1.0.0.81_all.deb
Description: Steam dependencies

",
        )
        .unwrap();

        let found: Vec<String> = search(&repo, "steam")
            .into_iter()
            .map(|p| p.package)
            .collect();
        assert_eq!(found, vec!["steam-installer", "steam-libs-i386"]);

        let by_desc: Vec<String> = search(&repo, "launcher")
            .into_iter()
            .map(|p| p.package)
            .collect();
        assert_eq!(by_desc, vec!["steam-installer"]);

        assert!(search(&repo, "zzz-nothing").is_empty());

        if let Some(old) = old_home {
            unsafe { std::env::set_var("HOME", old) };
        } else {
            unsafe { std::env::remove_var("HOME") };
        }
        let _ = fs::remove_dir_all(&tmp);
    }
}
