//! Declarative `.kdl` configuration: declares repos and the packages to
//! install from them. Installs flow through the same path as `univ install`,
//! so packages are merged into the nspawn container tree and get launchers.

use crate::deb::DebMeta;
use crate::store::{Store, StorePath};
use crate::term;
use crate::{link, repo, rpmrepo};
use std::io;
use std::path::Path;

/// Apply a declarative `.kdl` configuration file.
pub fn process_file(path: &str) -> io::Result<()> {
    let content = std::fs::read_to_string(path)?;
    let doc: kdl::KdlDocument = content
        .parse()
        .map_err(|e| io::Error::other(format!("invalid kdl in {path}: {e}")))?;

    let mut errors: Vec<String> = Vec::new();
    for node in doc.nodes() {
        match node.name().value() {
            "repo" => {
                if let Err(e) = process_repo(node) {
                    errors.push(e.to_string());
                }
            }
            "import" => {
                let target = node.get(0).and_then(|v| v.as_string()).unwrap_or("?");
                eprintln!(
                    "{} import not yet implemented: {}",
                    term::yellow("notice"),
                    target
                );
            }
            other => {
                eprintln!(
                    "{} unknown config node '{other}'",
                    term::yellow("warning")
                );
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "config had errors: {}",
            errors.join("; ")
        )))
    }
}

fn process_repo(node: &kdl::KdlNode) -> io::Result<()> {
    let name = node
        .get(0)
        .and_then(|v| v.as_string())
        .ok_or_else(|| io::Error::other("repo node is missing a name argument"))?;
    let base = match node.get(1).and_then(|v| v.as_string()) {
        Some(b) => Some(b.to_string()),
        None => existing_base(name),
    };
    let base = base.ok_or_else(|| {
        io::Error::other(format!(
            "repo '{name}' has no base URL and is not configured yet"
        ))
    })?;
    let arches: Vec<String> = node
        .entries()
        .iter()
        .skip(2)
        .filter_map(|e| e.value().as_string().map(str::to_string))
        .collect();

    let want_deb = is_deb_base(&base) || !is_rpm_base(&base);
    let want_rpm = is_rpm_base(&base) || !is_deb_base(&base);

    if want_deb {
        repo::add_repo(name, &base, &arches)?;
        if let Some(r) = repo::repos()?.into_iter().find(|r| r.name == name) {
            match repo::update(&r) {
                Ok(n) => eprintln!(
                    "{} {name}: {n} deb packages indexed",
                    term::green("updated")
                ),
                Err(e) => eprintln!(
                    "{} failed to update deb repo '{name}': {e}",
                    term::warn("warning:")
                ),
            }
        }
    }
    if want_rpm {
        rpmrepo::add_repo(name, &base, &rpm_arches(&arches))?;
        if let Some(r) = rpmrepo::repos()?.into_iter().find(|r| r.name == name) {
            match rpmrepo::update(&r) {
                Ok(n) => eprintln!(
                    "{} {name}: {n} rpm packages indexed",
                    term::green("updated")
                ),
                Err(e) => eprintln!(
                    "{} failed to update rpm repo '{name}': {e}",
                    term::warn("warning:")
                ),
            }
        }
    }

    if let Some(children) = node.children() {
        let store = Store::open()?;
        let pkgs: Vec<String> = children.nodes().iter().map(|c| c.name().value().into()).collect();
        install_packages(&store, name, &pkgs, want_deb, want_rpm)?;
    }
    Ok(())
}

fn install_packages(
    store: &Store,
    repo_name: &str,
    pkgs: &[String],
    want_deb: bool,
    want_rpm: bool,
) -> io::Result<()> {
    for pkg in pkgs {
        let mut installed = false;
        if want_deb
            && let Some(r) = repo::repos()?.into_iter().find(|r| r.name == repo_name)
        {
            match repo::install(store, &r, pkg) {
                Ok(out) => {
                    installed = true;
                    link_all(store, &out);
                }
                Err(e) => {
                    if !want_rpm {
                        return Err(e);
                    }
                    eprintln!(
                        "{} deb '{pkg}' from '{repo_name}': {e}",
                        term::warn("warning:")
                    );
                }
            }
        }
        if want_rpm
            && !installed
            && let Some(r) = rpmrepo::repos()?.into_iter().find(|r| r.name == repo_name)
        {
            let out = rpmrepo::install(store, &r, pkg)?;
            installed = true;
            link_all(store, &out);
        }
        if !installed {
            return Err(io::Error::other(format!(
                "no configured repo '{repo_name}' for package '{pkg}'"
            )));
        }
    }
    Ok(())
}

/// Record installs in the store *and* integrate them with the nspawn container:
/// merge the files into the container tree and create host launchers.
fn link_all(store: &Store, installed: &[(StorePath, DebMeta)]) {
    for (sp, meta) in installed {
        println!(
            "{} {}-{} [{}]",
            term::bold_green("installed"),
            term::bold_cyan(&meta.package),
            term::green(&meta.version),
            term::magenta(&meta.architecture)
        );
        if let Err(e) = link::link_package(store, sp, meta) {
            eprintln!(
                "{} integration failed for {}: {e}",
                term::warn("warning:"),
                meta.package
            );
        }
    }
}

fn existing_base(name: &str) -> Option<String> {
    for file in [".local/share/univ/debrepos.conf", ".local/share/univ/rpmrepos.conf"] {
        let home = std::env::var_os("HOME")?;
        let path = Path::new(&home).join(file);
        if let Ok(text) = std::fs::read_to_string(path) {
            for line in text.lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 && parts[0] == name {
                    return Some(parts[1].to_string());
                }
            }
        }
    }
    None
}

fn rpm_arches(arches: &[String]) -> Vec<String> {
    arches
        .iter()
        .map(|a| match a.as_str() {
            "amd64" => "x86_64".to_string(),
            "i386" => "i686".to_string(),
            other => other.to_string(),
        })
        .collect()
}

fn is_deb_base(base: &str) -> bool {
    base.contains("debian.org")
        || base.contains("ubuntu.com")
        || base.contains("linuxmint")
        || base.contains("/dists/")
        || base.contains("/pool/")
        || base.ends_with("/debian")
}

fn is_rpm_base(base: &str) -> bool {
    base.contains("fedoraproject.org")
        || base.contains("rpmfusion")
        || base.contains("repodata")
        || base.contains("kojipkgs")
        || base.contains("epel")
        || base.contains("/os/")
}
