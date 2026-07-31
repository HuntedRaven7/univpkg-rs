//! Fedora / RPM-based repository support.
//!
//! Fedora (and RHEL/CentOS/Rocky/Alma) repos use the DNF/createrepo layout:
//!
//! 1. `repomd.xml` — an XML manifest that lists the location of the primary
//!    database, their type, and checksums.
//! 2. `primary.xml.gz` (or `.zst`) — an XML file with one `<package>` stanza
//!    per package containing name, version, arch, dependencies, download URL,
//!    and checksum.
//!
//! We parse the minimal subset of both formats we need with a simple
//! hand-rolled XML scanner (no external XML library required).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;

use crate::deb::DebMeta;
use crate::resolve;
use crate::rpm::{self, RpmDep};
use crate::store::{sha256_hex, Store, StorePath};
use crate::version;

// ── Constants ─────────────────────────────────────────────────────────────────

const DEFAULT_FEDORA_REPO_NAME: &str = "fedora";

/// Default Fedora release version (e.g. "44", "43", "rawhide").
pub const DEFAULT_FEDORA_VERSION: &str = "44";

/// Custom repository base URL override. Set to `Some("https://...")` to specify
/// a custom repository URL directly in the code (e.g., custom mirror or local repo),
/// or `None` to construct the base URL automatically using `DEFAULT_FEDORA_VERSION`.
pub const CUSTOM_FEDORA_BASE_URL: Option<&str> = None;

/// Construct the default repository base URL for Fedora.
pub fn default_fedora_base_url() -> String {
    if let Some(url) = CUSTOM_FEDORA_BASE_URL {
        url.to_string()
    } else {
        format!(
            "https://dl.fedoraproject.org/pub/fedora/linux/releases/{}/Everything/{}/os",
            DEFAULT_FEDORA_VERSION,
            host_arch()
        )
    }
}

/// RPM packages that are part of every Fedora base system and do not need to
/// be downloaded.  Mirrors the logic in `repo.rs` for Debian.
const SYSTEM_PKGS: &[&str] = &[
    "glibc",
    "glibc-common",
    "glibc-minimal-langpack",
    "libgcc",
    "libstdc++",
    "zlib",
    "xz-libs",
    "zstd",
    "bzip2-libs",
    "lz4-libs",
    "libffi",
    "libselinux",
    "pcre2",
    "pam",
    "libcap",
    "audit-libs",
    "util-linux",
    "systemd-libs",
    "expat",
    "libX11",
    "libxcb",
    "libXau",
    "libXdmcp",
    "libXext",
    "libXinerama",
    "libXrandr",
    "libXrender",
    "libXScrnSaver",
    "libXt",
    "libSM",
    "libICE",
    "libXmu",
    "libXpm",
    "libXft",
    "libXi",
    "libXcursor",
    "fontconfig",
    "freetype",
    "libpng",
    "libjpeg-turbo",
    "libxkbcommon",
    "glib2",
    "cairo",
    "pango",
    "atk",
    "gdk-pixbuf2",
    "gtk3",
    "wayland-libs",
    "dbus-libs",
    "nspr",
    "nss",
    "setup",
    "filesystem",
    "basesystem",
    "dnf",
    "rpm",
    "rpm-libs",
    "coreutils",
    "bash",
];

/// Host architecture in Fedora/RPM terminology.
pub fn host_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "x86" => "i686",
        "aarch64" => "aarch64",
        other => other,
    }
}

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct FedoraRepo {
    pub name: String,
    pub base: String,
    pub arches: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct RpmPackage {
    pub package: String,
    #[allow(dead_code)]
    pub version: String,
    /// Full EVR as it appears in repodata (e.g. `1.0-1.fc41`).
    pub full_version: String,
    pub epoch: u32,
    pub architecture: String,
    pub description: String,
    pub requires: Vec<Vec<RpmDep>>,
    pub location: String, // relative href in the repo
    pub sha256: Option<String>,
}

#[derive(Clone, Debug, Default)]
struct Index {
    by_name: HashMap<String, Vec<RpmPackage>>,
}

impl Index {
    fn insert(&mut self, pkg: RpmPackage) {
        self.by_name.entry(pkg.package.clone()).or_default().push(pkg);
    }

    fn merge(&mut self, other: Index) {
        for (name, mut pkgs) in other.by_name {
            self.by_name.entry(name).or_default().append(&mut pkgs);
        }
    }

    fn candidates(&self, name: &str, arch: &str) -> Vec<RpmPackage> {
        self.by_name
            .get(name)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|p| p.architecture == "noarch" || p.architecture == arch)
            .collect()
    }
}

// ── Repo list ─────────────────────────────────────────────────────────────────

/// Return the list of configured RPM repositories.  Config is in
/// `~/.local/unipkg/rpmrepos.conf`.  Each line: `name url [arch…]`.
/// Falls back to Fedora 41 if no config exists.
pub fn repos() -> io::Result<Vec<FedoraRepo>> {
    let conf = Store::root()?.join("rpmrepos.conf");
    let mut out = Vec::new();
    if let Ok(text) = fs::read_to_string(&conf) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut it = line.split_whitespace();
            let (Some(name), Some(base)) = (it.next(), it.next()) else { continue };
            let arches: Vec<String> = it.map(str::to_string).collect();
            out.push(FedoraRepo {
                name: name.to_string(),
                base: base.to_string(),
                arches: if arches.is_empty() {
                    vec![host_arch().to_string()]
                } else {
                    arches
                },
            });
        }
    }
    if out.is_empty() {
        out.push(FedoraRepo {
            name: DEFAULT_FEDORA_REPO_NAME.to_string(),
            base: default_fedora_base_url(),
            arches: vec![host_arch().to_string()],
        });
    }
    Ok(out)
}

/// Append a new RPM repository entry to `~/.local/unipkg/rpmrepos.conf`.
/// `arches` may be empty to default to the host architecture.
pub fn add_repo(name: &str, base: &str, arches: &[String]) -> io::Result<()> {
    let conf = Store::root()?.join("rpmrepos.conf");
    if let Some(parent) = conf.parent() {
        fs::create_dir_all(parent)?;
    }
    let arches_part = if arches.is_empty() {
        host_arch().to_string()
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

// ── Update: fetch & cache repodata ───────────────────────────────────────────

fn cache_path(repo: &FedoraRepo, arch: &str) -> io::Result<PathBuf> {
    Ok(Store::root()?
        .join("cache")
        .join(format!("{}.rpm-primary.{arch}", repo.name)))
}

/// Fetch and cache the primary package index for each configured architecture.
pub fn update(repo: &FedoraRepo) -> io::Result<usize> {
    let mut total = 0;
    for arch in &repo.arches {
        let base = repo.base.trim_end_matches('/');
        let repomd_url = format!("{base}/repodata/repomd.xml");
        let repomd_bytes = http_get(&repomd_url)?;
        let repomd_text =
            String::from_utf8(repomd_bytes).map_err(io::Error::other)?;

        // Find the href of the primary database.
        let primary_href = find_primary_href(&repomd_text).ok_or_else(|| {
            io::Error::other(format!(
                "no primary database found in repomd.xml for repo '{}'",
                repo.name
            ))
        })?;

        let primary_url = if primary_href.starts_with("http") {
            primary_href.clone()
        } else {
            format!("{base}/{}", primary_href.trim_start_matches('/'))
        };

        let primary_bytes = http_get(&primary_url)?;
        let primary_text = decompress_primary(&primary_url, &primary_bytes)?;

        let index = parse_primary(&primary_text);
        let n = index.by_name.values().map(|v| v.len()).sum::<usize>();

        let path = cache_path(repo, arch)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, render_index(&index))?;

        println!("{} ({arch}): {n} packages", repo.name);
        total += n;
    }
    Ok(total)
}

/// Extract the `href` of the `type="primary"` entry from `repomd.xml`.
fn find_primary_href(repomd: &str) -> Option<String> {
    // We look for a `<data type="primary">` block and grab the `<location href=...>`.
    let mut in_primary = false;
    for line in repomd.lines() {
        let t = line.trim();
        if t.contains("type=\"primary\"") || t.contains("type='primary'") {
            in_primary = true;
        }
        if in_primary && t.contains("</data>") {
            break;
        }
        if in_primary {
            if let Some(href) = extract_attr(t, "location", "href") {
                return Some(href);
            }
        }
    }
    None
}

fn decompress_primary(url: &str, bytes: &[u8]) -> io::Result<String> {
    use std::io::Cursor;
    if url.ends_with(".gz") || url.ends_with(".xml.gz") {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(Cursor::new(bytes)).read_to_end(&mut out)?;
        String::from_utf8(out).map_err(io::Error::other)
    } else if url.ends_with(".zst") || url.ends_with(".xml.zst") {
        let mut out = Vec::new();
        ruzstd::decoding::StreamingDecoder::new(Cursor::new(bytes))
            .map_err(|e| io::Error::other(format!("zstd: {e}")))?
            .read_to_end(&mut out)?;
        String::from_utf8(out).map_err(io::Error::other)
    } else if url.ends_with(".xz") || url.ends_with(".xml.xz") {
        let mut out = Vec::new();
        lzma_rs::xz_decompress(&mut Cursor::new(bytes), &mut out)
            .map_err(|e| io::Error::other(format!("xz: {e}")))?;
        String::from_utf8(out).map_err(io::Error::other)
    } else {
        String::from_utf8(bytes.to_vec()).map_err(io::Error::other)
    }
}

// ── primary.xml parsing ───────────────────────────────────────────────────────

/// Parse a `primary.xml` document into an index.
///
/// The format is structured XML but we parse it with a simple line-by-line
/// scanner rather than pulling in an XML library.
fn parse_primary(text: &str) -> Index {
    let mut index = Index::default();
    let mut pkg = PkgBuilder::default();
    let mut in_version = false;
    let mut in_req = false;

    for line in text.lines() {
        let t = line.trim();

        if t.starts_with("<package ") || t == "<package>" {
            pkg = PkgBuilder::default();
        } else if t.starts_with("</package>") {
            if let Some(p) = pkg.build() {
                index.insert(p);
            }
            pkg = PkgBuilder::default();
        } else if t.starts_with("<name>") {
            pkg.name = inner_text(t, "name").map(str::to_string);
        } else if t.starts_with("<arch>") {
            pkg.arch = inner_text(t, "arch").map(str::to_string);
        } else if t.starts_with("<summary>") {
            pkg.summary = inner_text(t, "summary").map(str::to_string);
        } else if t.starts_with("<version ") {
            // <version epoch="0" ver="1.0" rel="1.fc41"/>
            pkg.epoch = extract_attr(t, "version", "epoch")
                .and_then(|e| e.parse().ok())
                .unwrap_or(0);
            pkg.ver = extract_attr(t, "version", "ver");
            pkg.rel = extract_attr(t, "version", "rel");
            in_version = true;
        } else if in_version && t.contains("</version>") {
            in_version = false;
        } else if t.starts_with("<location ") {
            pkg.location = extract_attr(t, "location", "href");
        } else if t.starts_with("<checksum ") && t.contains("sha256") {
            pkg.sha256 = inner_text(t, "checksum").map(str::to_string);
        } else if t.starts_with("<requires>") {
            in_req = true;
        } else if t.starts_with("</requires>") {
            in_req = false;
        } else if in_req && (t.starts_with("<entry ") || t.starts_with("<rpm:entry ")) {
            if let Some(dep) = parse_dep_entry(t) {
                pkg.requires.push(vec![dep]);
            }
        }
    }

    index
}

#[derive(Default)]
struct PkgBuilder {
    name: Option<String>,
    arch: Option<String>,
    summary: Option<String>,
    epoch: u32,
    ver: Option<String>,
    rel: Option<String>,
    location: Option<String>,
    sha256: Option<String>,
    requires: Vec<Vec<RpmDep>>,
}

impl PkgBuilder {
    fn build(self) -> Option<RpmPackage> {
        let package = self.name?;
        let ver = self.ver?;
        let rel = self.rel.unwrap_or_default();
        let full_version = if rel.is_empty() {
            ver.clone()
        } else {
            format!("{ver}-{rel}")
        };
        let location = self.location?;
        Some(RpmPackage {
            package,
            version: ver,
            full_version,
            epoch: self.epoch,
            architecture: self.arch.unwrap_or_default(),
            description: self.summary.unwrap_or_default(),
            requires: self.requires,
            location,
            sha256: self.sha256,
        })
    }
}

/// Parse one `<entry name="..." flags="..." ver="..."/>` dependency element.
fn parse_dep_entry(line: &str) -> Option<RpmDep> {
    let name = extract_attr(line, "entry", "name")?;
    // Skip file deps, rpmlib pseudo-deps, config, and rich-deps.
    if name.starts_with('/') || name.starts_with("rpmlib(") || name.starts_with('(') {
        return None;
    }
    let flags_str = extract_attr(line, "entry", "flags").unwrap_or_default();
    let ver = extract_attr(line, "entry", "ver");
    let version = match (flags_str.as_str(), ver) {
        ("", _) | (_, None) => None,
        (flags, Some(v)) => {
            let op = match flags {
                "LT" => "<",
                "GT" => ">",
                "EQ" => "=",
                "LE" => "<=",
                "GE" => ">=",
                _ => return Some(RpmDep { package: name, version: None }),
            };
            Some((op.to_string(), v))
        }
    };
    Some(RpmDep { package: name, version })
}

// ── Index serialisation ───────────────────────────────────────────────────────

fn render_index(index: &Index) -> String {
    let mut all: Vec<&RpmPackage> = index.by_name.values().flatten().collect();
    all.sort_by(|a, b| a.package.cmp(&b.package).then(a.full_version.cmp(&b.full_version)));
    let mut out = String::new();
    for pkg in all {
        out.push_str(&format!("Package: {}\n", pkg.package));
        out.push_str(&format!("Version: {}\n", pkg.full_version));
        out.push_str(&format!("Architecture: {}\n", pkg.architecture));
        if pkg.epoch != 0 {
            out.push_str(&format!("Epoch: {}\n", pkg.epoch));
        }
        if !pkg.description.is_empty() {
            out.push_str(&format!("Description: {}\n", pkg.description));
        }
        if !pkg.requires.is_empty() {
            out.push_str(&format!(
                "Requires: {}\n",
                rpm::render_requires(&pkg.requires)
            ));
        }
        out.push_str(&format!("Filename: {}\n", pkg.location));
        if let Some(h) = &pkg.sha256 {
            out.push_str(&format!("SHA256: {h}\n"));
        }
        out.push('\n');
    }
    out
}

fn parse_index(text: &str) -> Index {
    let mut index = Index::default();
    let mut stanza: HashMap<&str, String> = HashMap::new();
    let mut cur_key: Option<&str> = None;

    let mut flush = |stanza: &mut HashMap<&str, String>| {
        if let Some(pkg) = stanza_to_package(stanza) {
            index.insert(pkg);
        }
        stanza.clear();
    };

    for line in text.lines() {
        if line.is_empty() {
            flush(&mut stanza);
            cur_key = None;
        } else if let Some((k, v)) = line.split_once(':') {
            let k = k.trim();
            stanza.insert(k, v.trim().to_string());
            cur_key = Some(k);
        } else if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(k) = cur_key {
                if let Some(v) = stanza.get_mut(k) {
                    v.push('\n');
                    v.push_str(line.trim_start());
                }
            }
        }
    }
    flush(&mut stanza);
    index
}

fn stanza_to_package(stanza: &HashMap<&str, String>) -> Option<RpmPackage> {
    let package = stanza.get("Package")?.clone();
    let full_version = stanza.get("Version")?.clone();
    let version = full_version.split('-').next().unwrap_or(&full_version).to_string();
    let location = stanza.get("Filename")?.clone();
    let epoch: u32 = stanza.get("Epoch").and_then(|e| e.parse().ok()).unwrap_or(0);
    let requires = stanza
        .get("Requires")
        .map(|r| rpm::parse_requires_field(r))
        .unwrap_or_default();
    Some(RpmPackage {
        package,
        version,
        full_version,
        epoch,
        architecture: stanza.get("Architecture").cloned().unwrap_or_default(),
        description: stanza.get("Description").cloned().unwrap_or_default(),
        requires,
        location,
        sha256: stanza.get("SHA256").cloned(),
    })
}

// ── Search ────────────────────────────────────────────────────────────────────

pub fn search(repo: &FedoraRepo, query: &str) -> Vec<RpmPackage> {
    let Ok(index) = read_index(repo) else { return Vec::new() };
    let q = query.to_lowercase();
    let mut out: Vec<RpmPackage> = index
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

fn read_index(repo: &FedoraRepo) -> io::Result<Index> {
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
            "no index for repo '{}'; run `unipkg update-rpm`",
            repo.name
        )));
    }
    Ok(index)
}

// ── Install ───────────────────────────────────────────────────────────────────

pub fn install(
    store: &Store,
    repo: &FedoraRepo,
    name: &str,
) -> io::Result<Vec<(StorePath, DebMeta)>> {
    let index = read_index(repo).map_err(|_| {
        io::Error::other(format!(
            "no index for repo '{}'; run `unipkg update-rpm`",
            repo.name
        ))
    })?;
    let installed = resolve::installed_packages(store);

    let mut plan: Vec<RpmPackage> = Vec::new();
    let mut planned: HashSet<(String, String)> = HashSet::new();
    plan_package(&installed, &index, name, None, None, &mut plan, &mut planned)?;

    let mut out = Vec::new();
    for pkg in &plan {
        println!(
            "  downloading {}-{} [{}]",
            pkg.package, pkg.full_version, pkg.architecture
        );
        let base = repo.base.trim_end_matches('/');
        let url = if pkg.location.starts_with("http") {
            pkg.location.clone()
        } else {
            format!("{base}/{}", pkg.location.trim_start_matches('/'))
        };
        let bytes = http_get(&url)?;
        if let Some(expected) = &pkg.sha256 {
            let got = sha256_hex(&bytes);
            if !got.eq_ignore_ascii_case(expected) {
                return Err(io::Error::other(format!(
                    "checksum mismatch for {}: expected {expected}, got {got}",
                    pkg.location
                )));
            }
        }
        let (sp, rpm_meta) = rpm::install(store, &bytes)?;
        rpm::write_meta(&rpm_meta, &sp)?;
        let deb_meta: DebMeta = rpm_meta.into();
        out.push((sp, deb_meta));
    }
    Ok(out)
}

// ── Dependency planner ────────────────────────────────────────────────────────

fn plan_package(
    installed: &[resolve::Installed],
    index: &Index,
    name: &str,
    arch: Option<&str>,
    constraint: Option<&(String, String)>,
    plan: &mut Vec<RpmPackage>,
    planned: &mut HashSet<(String, String)>,
) -> io::Result<()> {
    let desired = arch
        .map(str::to_owned)
        .unwrap_or_else(|| host_arch().to_owned());

    if SYSTEM_PKGS.contains(&name) && desired == host_arch() {
        planned.insert((name.to_string(), desired));
        return Ok(());
    }

    // Already installed?
    if let Some(p) = installed.iter().find(|p| {
        p.meta.package == name
            && (p.meta.architecture == desired || p.meta.architecture == "noarch")
            && constraint_ok(&p.meta.version, constraint)
    }) {
        planned.insert((p.meta.package.clone(), p.meta.architecture.clone()));
        return Ok(());
    }

    let candidate = best_candidate(index, name, &desired, constraint).ok_or_else(|| {
        io::Error::other(format!("cannot satisfy dependency '{name}'"))
    })?;

    let key = (candidate.package.clone(), candidate.architecture.clone());
    if !planned.insert(key) {
        return Ok(());
    }

    for group in &candidate.requires {
        let mut chosen: Option<io::Error> = None;
        for alt in group {
            let dep_arch = desired.clone();
            let mut sub_plan = Vec::new();
            let mut sub_planned = planned.clone();
            match plan_package(
                installed,
                index,
                &alt.package,
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
) -> Option<RpmPackage> {
    index
        .candidates(name, arch)
        .into_iter()
        .filter(|p| constraint_ok(&p.full_version, constraint))
        .max_by(|a, b| version::compare(&a.full_version, &b.full_version))
}

fn constraint_ok(version: &str, c: Option<&(String, String)>) -> bool {
    match c {
        None => true,
        Some((op, req)) => version::satisfies(version, op, req),
    }
}

// ── HTTP helper ───────────────────────────────────────────────────────────────

fn http_get(url: &str) -> io::Result<Vec<u8>> {
    match ureq::get(url).call() {
        Ok(mut res) => res
            .body_mut()
            .with_config()
            .limit(MAX_BODY.try_into().unwrap())
            .read_to_vec()
            .map_err(io::Error::other),
        Err(ureq::Error::StatusCode(404)) => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{url}: not found"),
        )),
        Err(e) => Err(io::Error::other(format!("{url}: {e}"))),
    }
}

const MAX_BODY: usize = 256 * 1024 * 1024; // 256 MiB

// ── XML mini-helpers ──────────────────────────────────────────────────────────

/// Extract the value of `attr` from an XML element whose tag name is `tag`.
/// Handles both single- and double-quoted attribute values.
fn extract_attr(line: &str, _tag: &str, attr: &str) -> Option<String> {
    // Look for `attr="value"` or `attr='value'`
    let key_dq = format!("{attr}=\"");
    let key_sq = format!("{attr}='");
    for (key, close) in [(&key_dq, '"'), (&key_sq, '\'')] {
        if let Some(start) = line.find(key.as_str()) {
            let after = start + key.len();
            if let Some(end) = line[after..].find(close) {
                return Some(line[after..after + end].to_string());
            }
        }
    }
    None
}

/// Extract the text content of a simple `<tag>content</tag>` element.
fn inner_text<'a>(line: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let start = line.find(&open)?;
    // Skip past the closing `>` of the open tag.
    let after_open = line[start..].find('>')? + start + 1;
    let end = line.find(&close)?;
    if end >= after_open {
        Some(&line[after_open..end])
    } else {
        None
    }
}



#[cfg(test)]
mod tests {
    use super::*;

    fn pkg(name: &str, ver: &str, requires: Vec<Vec<RpmDep>>) -> RpmPackage {
        RpmPackage {
            package: name.into(),
            version: ver.into(),
            full_version: format!("{ver}-1"),
            epoch: 0,
            architecture: "x86_64".into(),
            description: String::new(),
            requires,
            location: format!("Packages/{name}-{ver}.x86_64.rpm"),
            sha256: None,
        }
    }

    fn idx(pkgs: Vec<RpmPackage>) -> Index {
        let mut index = Index::default();
        for p in pkgs {
            index.insert(p);
        }
        index
    }

    fn plan_names(index: &Index, name: &str) -> Vec<String> {
        let mut plan = Vec::new();
        let mut planned = HashSet::new();
        plan_package(&[], index, name, None, None, &mut plan, &mut planned).unwrap();
        plan.iter().map(|p| p.package.clone()).collect()
    }

    #[test]
    fn picks_highest_version() {
        let index = idx(vec![
            pkg("foo", "1.0", vec![]),
            pkg("foo", "2.0", vec![]),
            pkg("foo", "1.5", vec![]),
        ]);
        assert_eq!(
            best_candidate(&index, "foo", "x86_64", None)
                .unwrap()
                .full_version,
            "2.0-1"
        );
    }

    #[test]
    fn plans_transitive_closure() {
        let index = idx(vec![
            pkg("app", "1.0", vec![
                vec![RpmDep::package_only("liba")],
                vec![RpmDep::package_only("libb")],
            ]),
            pkg("liba", "2.0", vec![vec![RpmDep::package_only("libc")]]),
            pkg("libb", "3.0", vec![]),
            pkg("libc", "4.0", vec![]),
        ]);
        assert_eq!(plan_names(&index, "app"), vec!["libc", "liba", "libb", "app"]);
    }

    #[test]
    fn system_packages_skipped() {
        let index = idx(vec![pkg("glibc", "2.40", vec![])]);
        assert!(plan_names(&index, "glibc").is_empty());
    }

    #[test]
    fn unsatisfied_dep_errors() {
        let index = idx(vec![pkg("app", "1.0", vec![vec![RpmDep::package_only("ghost")]])]);
        let mut plan = Vec::new();
        let mut planned = HashSet::new();
        let err = plan_package(&[], &index, "app", None, None, &mut plan, &mut planned);
        assert!(err.is_err());
    }

    #[test]
    fn parse_primary_xml() {
        let xml = r#"<?xml version="1.0"?>
<metadata>
  <package type="rpm">
    <name>bash</name>
    <arch>x86_64</arch>
    <version epoch="0" ver="5.2.21" rel="4.fc41"/>
    <summary>The GNU Bourne Again shell</summary>
    <location href="Packages/b/bash-5.2.21-4.fc41.x86_64.rpm"/>
    <checksum type="sha256">abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890ab</checksum>
    <requires>
      <entry name="glibc" flags="GE" ver="2.38"/>
      <entry name="/bin/sh"/>
    </requires>
  </package>
</metadata>"#;
        let index = parse_primary(xml);
        let pkgs = index.by_name.get("bash").unwrap();
        assert_eq!(pkgs.len(), 1);
        let p = &pkgs[0];
        assert_eq!(p.package, "bash");
        assert_eq!(p.full_version, "5.2.21-4.fc41");
        assert_eq!(p.architecture, "x86_64");
        assert!(p.location.contains("bash-5.2.21"));
        // /bin/sh dep must be filtered out; glibc dep kept.
        assert_eq!(p.requires.len(), 1);
        assert_eq!(p.requires[0][0].package, "glibc");
        assert_eq!(
            p.requires[0][0].version,
            Some((">=".into(), "2.38".into()))
        );
    }

    #[test]
    fn repomd_href_extraction() {
        let xml = r#"<?xml version="1.0"?>
<repomd>
  <data type="primary">
    <location href="repodata/abc123-primary.xml.gz"/>
    <checksum type="sha256">deadbeef</checksum>
  </data>
  <data type="filelists">
    <location href="repodata/def456-filelists.xml.gz"/>
  </data>
</repomd>"#;
        let href = find_primary_href(xml).unwrap();
        assert_eq!(href, "repodata/abc123-primary.xml.gz");
    }

    #[test]
    fn index_render_roundtrip() {
        let index = idx(vec![
            pkg("foo", "1.0", vec![vec![RpmDep::package_only("bar")]]),
            pkg("bar", "2.0", vec![]),
        ]);
        let rendered = render_index(&index);
        let parsed = parse_index(&rendered);
        assert!(parsed.by_name.contains_key("foo"));
        assert!(parsed.by_name.contains_key("bar"));
        let foo = &parsed.by_name["foo"][0];
        assert_eq!(foo.full_version, "1.0-1");
        assert_eq!(foo.requires.len(), 1);
        assert_eq!(foo.requires[0][0].package, "bar");
    }

    #[test]
    fn search_by_name_and_desc() {
        let _g = crate::store::TEST_HOME_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir()
            .join(format!("unipkg-rpmrepo-search-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &tmp);

        let repo = FedoraRepo {
            name: "fedora".into(),
            base: "http://example.invalid".into(),
            arches: vec!["x86_64".into()],
        };
        let cache_dir = tmp.join(".local/unipkg/cache");
        fs::create_dir_all(&cache_dir).unwrap();
        let index = idx(vec![
            pkg("vim-enhanced", "9.1", vec![]),
            pkg("vim-common", "9.1", vec![]),
            pkg("emacs", "30.0", vec![]),
        ]);
        fs::write(
            cache_dir.join("fedora.rpm-primary.x86_64"),
            render_index(&index),
        )
        .unwrap();

        let found: Vec<String> = search(&repo, "vim")
            .into_iter()
            .map(|p| p.package)
            .collect();
        assert!(found.contains(&"vim-enhanced".to_string()));
        assert!(found.contains(&"vim-common".to_string()));
        assert!(!found.contains(&"emacs".to_string()));

        if let Some(old) = old_home {
            std::env::set_var("HOME", old);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn default_fedora_url_construction() {
        let url = default_fedora_base_url();
        assert!(url.contains("/releases/44/Everything/"));
        assert!(url.ends_with("/os"));
    }
}
