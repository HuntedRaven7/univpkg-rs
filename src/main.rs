mod deb;
mod elf;
mod link;
mod repo;
mod resolve;
mod rpm;
mod rpmrepo;
mod store;
mod version;

use std::io;
use std::process::ExitCode;

struct SearchResult {
    package: String,
    version: String,
    architecture: String,
    kind: &'static str,
    _repo: String,
    description: String,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let command = match args.first() {
        None => {
            print_help();
            return ExitCode::FAILURE;
        }
        Some(c) if c == "-h" || c == "--help" => {
            print_help();
            return ExitCode::SUCCESS;
        }
        Some(c) => c.as_str(),
    };

    match command {
        "init" => match store::Store::init() {
            Ok(s) => {
                println!("initialized store at {}", s.base().display());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("unipkg: {e}");
                ExitCode::FAILURE
            }
        },
        "status" => match store::Store::open() {
            Ok(s) => {
                println!("store: {}", s.base().display());
                println!("store paths:");
                for p in s.paths().unwrap_or_default() {
                    println!("  {p}");
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("unipkg: {e}");
                ExitCode::FAILURE
            }
        },
        "add" => {
            let file = match args.get(1) {
                Some(f) => f,
                None => {
                    eprintln!("usage: unipkg add <file>");
                    return ExitCode::FAILURE;
                }
            };
            let bytes = match std::fs::read(file) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("unipkg: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let name = std::path::Path::new(file)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "unknown".to_string());
            let result = store::Store::open().and_then(|s| {
                let p = s.add(&bytes, &name)?;
                Ok((s, p))
            });
            match result {
                Ok((s, p)) => {
                    println!("added {}", s.base().join(p.to_string()).display());
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("unipkg: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        "add-repo" => {
            let rest: Vec<&str> = args[1..].iter().map(String::as_str).collect();
            if rest.len() < 2 {
                eprintln!("usage: unipkg add-repo <name> <base-url> [arch ...]");
                return ExitCode::FAILURE;
            }
            let (name, base, arches) = parse_repo_args(&rest);
            match repo::add_repo(&name, &base, &arches) {
                Ok(()) => {
                    println!("added deb repo '{name}' -> {base}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("unipkg: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        "add-rpm-repo" => {
            let rest: Vec<&str> = args[1..].iter().map(String::as_str).collect();
            if rest.len() < 2 {
                eprintln!("usage: unipkg add-rpm-repo <name> <base-url> [arch ...]");
                return ExitCode::FAILURE;
            }
            let (name, base, arches) = parse_repo_args(&rest);
            match rpmrepo::add_repo(&name, &base, &arches) {
                Ok(()) => {
                    println!("added rpm repo '{name}' -> {base}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("unipkg: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        "update" => {
            let repos = match repo::repos() {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("unipkg: {e}");
                    return ExitCode::FAILURE;
                }
            };
            for r in &repos {
                if let Err(e) = repo::update(r) {
                    eprintln!("unipkg: {e}");
                    return ExitCode::FAILURE;
                }
            }
            ExitCode::SUCCESS
        }
        "install" => {
            let arg = match args.get(1) {
                Some(f) => f,
                None => {
                    eprintln!("usage: unipkg install <file.deb | file.rpm | package>");
                    return ExitCode::FAILURE;
                }
            };
            if std::path::Path::new(arg).is_file() {
                if arg.ends_with(".rpm") {
                    return install_rpm_file(arg);
                }
                return install_deb_file(arg);
            }
            install_package(arg)
        }
        "install-deb" => {
            let arg = match args.get(1) {
                Some(f) => f,
                None => {
                    eprintln!("usage: unipkg install-deb <package>");
                    return ExitCode::FAILURE;
                }
            };
            install_package(arg)
        }
        "install-rpm" => {
            let arg = match args.get(1) {
                Some(f) => f,
                None => {
                    eprintln!("usage: unipkg install-rpm <package | file.rpm>");
                    return ExitCode::FAILURE;
                }
            };
            if std::path::Path::new(arg).is_file() {
                return install_rpm_file(arg);
            }
            install_rpm_package(arg)
        }
        "update-rpm" => {
            let repos = match rpmrepo::repos() {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("unipkg: {e}");
                    return ExitCode::FAILURE;
                }
            };
            for r in &repos {
                if let Err(e) = rpmrepo::update(r) {
                    eprintln!("unipkg: {e}");
                    return ExitCode::FAILURE;
                }
            }
            ExitCode::SUCCESS
        }
        "search" => {
            let query = match args.get(1) {
                Some(q) => q,
                None => {
                    eprintln!("usage: unipkg search <query>");
                    return ExitCode::FAILURE;
                }
            };
            let mut results: Vec<SearchResult> = Vec::new();
            if let Ok(repos) = repo::repos() {
                for r in &repos {
                    for p in repo::search(r, query) {
                        results.push(SearchResult {
                            package: p.package,
                            version: p.version,
                            architecture: p.architecture,
                            kind: "deb",
                            _repo: r.name.clone(),
                            description: p.description,
                        });
                    }
                }
            }
            if let Ok(rpm_repos) = rpmrepo::repos() {
                for r in &rpm_repos {
                    for p in rpmrepo::search(r, query) {
                        results.push(SearchResult {
                            package: p.package,
                            version: p.full_version,
                            architecture: p.architecture,
                            kind: "rpm",
                            _repo: r.name.clone(),
                            description: p.description,
                        });
                    }
                }
            }
            results.sort_by(|a, b| {
                a.package
                    .cmp(&b.package)
                    .then(a.kind.cmp(b.kind))
                    .then(a.architecture.cmp(&b.architecture))
            });
            results.dedup_by(|a, b| {
                a.package == b.package
                    && a.version == b.version
                    && a.architecture == b.architecture
                    && a.kind == b.kind
            });
            if results.is_empty() {
                println!("no packages match '{query}'");
                return ExitCode::FAILURE;
            }
            let shown = results.len().min(100);
            for p in &results[..shown] {

                const RESET: &str = "\x1b[0m";
                const BOLD_CYAN: &str = "\x1b[1;36m";
                const GREEN: &str = "\x1b[32m";
                const YELLOW: &str = "\x1b[33m";
                const MAGENTA: &str = "\x1b[35m";
                const DIM: &str = "\x1b[2m";
                let desc_part = if p.description.is_empty() {
                    String::new()
                } else {
                    format!(" - {}{}{}", DIM, p.description, RESET)
                };

                let (pkg_color, ver_color) = if p.kind == "rpm" {
                    (YELLOW, GREEN)
                } else {
                    (BOLD_CYAN, GREEN)
                };
                println!(
                    "{}{}{} {}{}{} [{}] {}{}{}{}",
                    pkg_color,
                    p.package,
                    RESET,
                    ver_color,
                    p.version,
                    RESET,
                    p.architecture,
                    MAGENTA,
                    p.kind,
                    RESET,
                    desc_part
                );
            }
            if results.len() > shown {
                println!("... and {} more", results.len() - shown);
            }
            ExitCode::SUCCESS
        }
        "deps" => {
            let package = match args.get(1) {
                Some(p) => p,
                None => {
                    eprintln!("usage: unipkg deps <package>");
                    return ExitCode::FAILURE;
                }
            };
            let store = match store::Store::open() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("unipkg: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let installed = resolve::installed_packages(&store);
            let pkg = match find_installed(&installed, package) {
                Some(p) => p,
                None => {
                    eprintln!("unipkg: no installed package matching '{package}'");
                    return ExitCode::FAILURE;
                }
            };
            let bins = link::find_binaries(&pkg.root).unwrap_or_default();
            if bins.is_empty() {
                println!("{}: no executables", pkg.meta.package);
            }
            for bin in bins {
                println!("{}:", bin.display());
                if let Some(interp) = resolve::interpreter(&bin) {
                    let status = if std::path::Path::new(&interp).exists() {
                        "ok"
                    } else {
                        "MISSING"
                    };
                    println!("  interpreter: {interp} ({status})");
                }
                for d in resolve::resolve_binary(&bin, &installed) {
                    let source = match &d.source {
                        resolve::LibSource::System(p) => {
                            format!("system: {}", p.display())
                        }
                        resolve::LibSource::Store { package, dir } => {
                            format!("store {package}: {}", dir.display())
                        }
                        resolve::LibSource::Missing => "MISSING".to_string(),
                    };
                    println!("  {} ({source})", d.name);
                }
            }
            ExitCode::SUCCESS
        }
        "rehash" => {
            let store = match store::Store::open() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("unipkg: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let installed = resolve::installed_packages(&store);
            let mut relinked = 0;
            for pkg in &installed {
                let _ = link::remove_artifacts(&pkg.sp);
                match link::link_package(&store, &pkg.sp, &pkg.meta) {
                    Ok(_) => {
                        relinked += 1;
                        println!("relinked {}", pkg.meta.package);
                    }
                    Err(e) => eprintln!(
                        "unipkg: warning: relink {}: {e}",
                        pkg.meta.package
                    ),
                }
            }
            println!("rehashed {relinked} package(s)");
            ExitCode::SUCCESS
        }
        "uninstall" => {
            let package = match args.get(1) {
                Some(p) => p,
                None => {
                    eprintln!("usage: unipkg uninstall <package>");
                    return ExitCode::FAILURE;
                }
            };
            match link::uninstall(package) {
                Ok((links, paths)) => {
                    println!(
                        "removed {links} link(s) and {paths} store path(s) for '{package}'"
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("unipkg: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        "unlink" => {
            let package = match args.get(1) {
                Some(p) => p,
                None => {
                    eprintln!("usage: unipkg unlink <package>");
                    return ExitCode::FAILURE;
                }
            };
            match store::Store::open().and_then(|_s| link::unlink(package)) {
                Ok(n) => {
                    println!("removed {n} link(s) for '{package}'");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("unipkg: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        other => {
            eprintln!("unipkg: unknown command '{other}'");
            ExitCode::FAILURE
        }
    }
}

fn install_package(arg: &str) -> ExitCode {
    let store = match store::Store::open() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("unipkg: {e}");
            return ExitCode::FAILURE;
        }
    };
    let repo = match repo::repos().and_then(|r| {
        r.first().cloned().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "no repositories configured")
        })
    }) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("unipkg: {e}");
            return ExitCode::FAILURE;
        }
    };
    match repo::install(&store, &repo, arg) {
        Ok(installed) => {
            for (sp, meta) in &installed {
                println!(
                    "installed {}-{} [{}]",
                    meta.package, meta.version, meta.architecture
                );
                let _ = link::link_package(&store, sp, meta);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("unipkg: {e}");
            ExitCode::FAILURE
        }
    }
}

fn print_help() {
    println!("unipkg - a content-addressed package manager");
    println!();
    println!("usage: unipkg <command> [args]");
    println!();
    println!("commands:");
    println!("  init                    create the store at ~/.local/unipkg");
    println!("  status                  list installed store paths");
    println!();
    println!("  -- Debian / Ubuntu (APT/deb) --");
    println!("  update                  refresh the deb package index from repos");
    println!("  add-repo <n> <url> [a…] append a deb repo to ~/.local/unipkg/debrepos.conf");
    println!("  search <query>          search cached package indexes (deb & rpm)");
    println!("  install <file.deb>      install a .deb from disk");
    println!("  install <package>       install a deb package (with deps) from a repo");
    println!("  install-deb <package>   install a deb package by name");
    println!();
    println!("  -- Fedora / RPM (DNF/rpm) --");
    println!("  update-rpm              refresh the RPM package index from repos");
    println!("  add-rpm-repo <n> <url>  append an RPM repo to ~/.local/unipkg/rpmrepos.conf");
    println!("  install <file.rpm>      install a .rpm from disk");
    println!("  install-rpm <package>   install an RPM package (with deps) from a repo");
    println!("  install-rpm <file.rpm>  install a .rpm from disk");
    println!();
    println!("  -- General --");
    println!("  deps <package>          show how a package's shared libraries resolve");
    println!("  rehash                  rebuild launchers for all installed packages");
    println!("  unlink <package>        remove a package's launchers and desktop entries");
    println!("  uninstall <package>     remove a package's files, launchers and store path");
}

fn install_deb_file(file: &str) -> ExitCode {
    let bytes = match std::fs::read(file) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("unipkg: {e}");
            return ExitCode::FAILURE;
        }
    };
    match store::Store::open().and_then(|s| {
        let (p, meta) = deb::install(&s, &bytes)?;
        deb::write_meta(&meta, &p)?;
        Ok((s, p, meta))
    }) {
        Ok((s, p, meta)) => {
            println!(
                "installed {}-{} [{}]",
                meta.package, meta.version, meta.architecture
            );
            println!("store: {}", s.base().join(p.to_string()).display());
            if !meta.description.is_empty() {
                let first = meta.description.lines().next().unwrap_or("");
                println!("  {first}");
            }
            if !meta.depends.is_empty() {
                println!("  depends: {}", deb::render_depends(&meta.depends));
            }
            match link::link_package(&s, &p, &meta) {
                Ok(linked) => {
                    if !linked.bin_links.is_empty() {
                        println!("linked into ~/.local/bin:");
                        for b in &linked.bin_links {
                            println!("  {}", b.display());
                        }
                        let bin_dir = store::Store::home_dir()
                            .map(|h| h.join(".local").join("bin"))
                            .unwrap_or_default();
                        if !on_path(&bin_dir) {
                            println!(
                                "note: {} is not on $PATH; add it to run these from a terminal",
                                bin_dir.display()
                            );
                        }
                    }
                    if !linked.desktop_files.is_empty() {
                        println!("desktop launchers:");
                        for d in &linked.desktop_files {
                            println!("  {}", d.display());
                        }
                    }
                    if !linked.icons.is_empty() {
                        println!("icons: {}", linked.icons.len());
                    }
                }
                Err(e) => eprintln!("unipkg: warning: integration failed: {e}"),
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("unipkg: {e}");
            ExitCode::FAILURE
        }
    }
}

fn install_rpm_file(file: &str) -> ExitCode {
    let bytes = match std::fs::read(file) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("unipkg: {e}");
            return ExitCode::FAILURE;
        }
    };
    match store::Store::open().and_then(|s| {
        let (p, rpm_meta) = rpm::install(&s, &bytes)?;
        rpm::write_meta(&rpm_meta, &p)?;
        let meta: deb::DebMeta = rpm_meta.into();
        Ok((s, p, meta))
    }) {
        Ok((s, p, meta)) => {
            println!(
                "installed {}-{} [{}]",
                meta.package, meta.version, meta.architecture
            );
            println!("store: {}", s.base().join(p.to_string()).display());
            if !meta.description.is_empty() {
                let first = meta.description.lines().next().unwrap_or("");
                println!("  {first}");
            }
            match link::link_package(&s, &p, &meta) {
                Ok(linked) => {
                    if !linked.bin_links.is_empty() {
                        println!("linked into ~/.local/bin:");
                        for b in &linked.bin_links {
                            println!("  {}", b.display());
                        }
                        let bin_dir = store::Store::home_dir()
                            .map(|h| h.join(".local").join("bin"))
                            .unwrap_or_default();
                        if !on_path(&bin_dir) {
                            println!(
                                "note: {} is not on $PATH; add it to run these from a terminal",
                                bin_dir.display()
                            );
                        }
                    }
                    if !linked.desktop_files.is_empty() {
                        println!("desktop launchers:");
                        for d in &linked.desktop_files {
                            println!("  {}", d.display());
                        }
                    }
                    if !linked.icons.is_empty() {
                        println!("icons: {}", linked.icons.len());
                    }
                }
                Err(e) => eprintln!("unipkg: warning: integration failed: {e}"),
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("unipkg: {e}");
            ExitCode::FAILURE
        }
    }
}

fn install_rpm_package(name: &str) -> ExitCode {
    let store = match store::Store::open() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("unipkg: {e}");
            return ExitCode::FAILURE;
        }
    };
    let repo = match rpmrepo::repos().and_then(|r| {
        r.into_iter().next().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "no RPM repositories configured")
        })
    }) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("unipkg: {e}");
            return ExitCode::FAILURE;
        }
    };
    match rpmrepo::install(&store, &repo, name) {
        Ok(installed) => {
            for (sp, meta) in &installed {
                println!(
                    "installed {}-{} [{}]",
                    meta.package, meta.version, meta.architecture
                );
                let _ = link::link_package(&store, sp, meta);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("unipkg: {e}");
            ExitCode::FAILURE
        }
    }
}

fn find_installed<'a>(installed: &'a [resolve::Installed], name: &str) -> Option<&'a resolve::Installed> {
    let (pkg, arch) = match name.rsplit_once(':') {
        Some((p, a)) => (p, Some(a)),
        None => (name, None),
    };
    installed
        .iter()
        .find(|p| {
            if p.meta.package != pkg {
                return false;
            }
            match arch {
                Some(a) => p.meta.architecture == a || p.meta.architecture == "all",
                None => true,
            }
        })
        .or_else(|| {
            installed.iter().find(|p| {
                p.sp.name() == name || p.sp.name().starts_with(&format!("{name}-"))
            })
        })
}

fn on_path(dir: &std::path::Path) -> bool {
    std::env::var("PATH")
        .map(|p| p.split(':').any(|d| !d.is_empty() && std::path::Path::new(d) == dir))
        .unwrap_or(false)
}

fn parse_repo_args(rest: &[&str]) -> (String, String, Vec<String>) {
    let name = rest[0].to_string();
    let base = rest[1].to_string();
    let arches: Vec<String> = rest[2..].iter().map(|s| s.to_string()).collect();
    (name, base, arches)
}
