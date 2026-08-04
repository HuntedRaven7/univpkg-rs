mod config;
mod deb;
mod elf;
mod link;
mod lock;
mod nspawn;
mod profile;
mod repo;
mod resolve;
mod rpm;
mod rpmrepo;
mod store;
mod term;
mod txn;
mod version;

use std::io;
use std::process::ExitCode;

struct SearchResult {
    package: String,
    version: String,
    architecture: String,
    kind: &'static str,
    repo: String,
    description: String,
    depends: String,
}

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let json = args.iter().any(|a| a == "--json");
    args.retain(|a| a != "--json");

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
        "--store" | "store" => run_store(),
        "init" => {
            let result: io::Result<()> = (|| {
                let _s = store::Store::init()?;
                nspawn::init(nspawn::ContainerKind::Deb)?;
                nspawn::init(nspawn::ContainerKind::Rpm)?;
                repo::ensure_default_repo()?;
                rpmrepo::ensure_default_repo()?;
                ensure_bashrc()?;
                Ok(())
            })();
            let runtime_result = ensure_crun_runtime();
            match result {
                Ok(()) => {
                    println!(
                        "{} {}",
                        term::bold_green("initialized"),
                        term::cyan(
                            &store::Store::root()
                                .map(|r| r.display().to_string())
                                .unwrap_or_default()
                        )
                    );
                    println!("{}", term::bold("containers:"));
                    for kind in [nspawn::ContainerKind::Deb, nspawn::ContainerKind::Rpm] {
                        println!(
                            "  {} -> {}",
                            term::bold_cyan(kind.name()),
                            term::cyan(
                                &nspawn::root(kind)
                                    .map(|r| r.display().to_string())
                                    .unwrap_or_default()
                            )
                        );
                    }
                    println!("{}", term::bold("configured repos:"));
                    for r in repo::repos().unwrap_or_default() {
                        println!(
                            "  {} {} {}",
                            term::bold_cyan(&r.name),
                            term::dim("->"),
                            term::cyan(&r.base)
                        );
                    }
                    for r in rpmrepo::repos().unwrap_or_default() {
                        println!(
                            "  {} {} {}",
                            term::bold_cyan(&r.name),
                            term::dim("->"),
                            term::cyan(&r.base)
                        );
                    }
                    let mut indexed = 0;
                    let mut failed = 0;
                    for r in repo::repos().unwrap_or_default() {
                        match repo::update(&r) {
                            Ok(n) => indexed += n,
                            Err(e) => {
                                failed += 1;
                                eprintln!(
                                    "{}",
                                    term::warn(&format!(
                                        "failed to update deb repo '{}': {e}",
                                        r.name
                                    ))
                                );
                            }
                        }
                    }
                    for r in rpmrepo::repos().unwrap_or_default() {
                        match rpmrepo::update(&r) {
                            Ok(n) => indexed += n,
                            Err(e) => {
                                failed += 1;
                                eprintln!(
                                    "{}",
                                    term::warn(&format!(
                                        "failed to update rpm repo '{}': {e}",
                                        r.name
                                    ))
                                );
                            }
                        }
                    }
                    if failed > 0 {
                        eprintln!(
                            "{}",
                            term::yellow(&format!(
                                "{failed} repo update(s) failed; run `univ update` / `univ update-rpm` to retry"
                            ))
                        );
                    } else {
                        println!("{} {indexed} packages", term::bold_green("indexed"));
                    }
                    match runtime_result {
                        Ok(()) => {}
                        Err(e) => {
                            eprintln!("{}", term::warn(&format!("bootstrap incomplete: {e}")))
                        }
                    }
                    for kind in [nspawn::ContainerKind::Deb, nspawn::ContainerKind::Rpm] {
                        let label = if nspawn::bootstrapped(kind) {
                            term::bold_green("bootstrapped").to_string()
                        } else {
                            term::yellow("no base OS yet (run `univ bootstrap`)").to_string()
                        };
                        println!(
                            "{} {}: {label}",
                            term::bold("container"),
                            term::bold_cyan(kind.name())
                        );
                    }
                    match nspawn::gpu_vendor() {
                        "nvidia" => println!(
                            "{} NVIDIA driver detected: launchers bind GPU devices and driver libs",
                            term::bold_green("gpu:")
                        ),
                        "nvidia (driver not loaded)" => println!(
                            "{} NVIDIA GPU detected but driver not loaded; launchers run without GPU passthrough",
                            term::yellow("gpu:")
                        ),
                        "intel" | "amd" => println!(
                            "{} {vendor} GPU detected",
                            term::bold_cyan("gpu:"),
                            vendor = nspawn::gpu_vendor()
                        ),
                        _ => println!(
                            "{} no GPU detected; launchers run without GPU passthrough",
                            term::yellow("gpu:")
                        ),
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("{}", term::error(&e.to_string()));
                    ExitCode::FAILURE
                }
            }
        }
        "apply" => {
            let path = match args.get(1) {
                Some(p) => p.clone(),
                None => match store::Store::root() {
                    Ok(r) => r.join("config.kdl").to_string_lossy().into_owned(),
                    Err(_) => "config.kdl".to_string(),
                },
            };
            match config::process_file(&path) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("{}", term::error(&e.to_string()));
                    ExitCode::FAILURE
                }
            }
        }
        "bootstrap" => match ensure_crun_runtime() {
            Ok(()) => {
                for kind in [nspawn::ContainerKind::Deb, nspawn::ContainerKind::Rpm] {
                    let label = if nspawn::bootstrapped(kind) {
                        term::bold_green("bootstrapped").to_string()
                    } else {
                        term::yellow("no base OS yet").to_string()
                    };
                    println!(
                        "{} {} -> {} ({label})",
                        term::bold("container"),
                        term::bold_cyan(kind.name()),
                        term::cyan(
                            &nspawn::root(kind)
                                .map(|r| r.display().to_string())
                                .unwrap_or_default()
                        )
                    );
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{}", term::error(&e.to_string()));
                ExitCode::FAILURE
            }
        },
        "status" => match store::Store::open() {
            Ok(s) => {
                println!(
                    "{} {}",
                    term::bold("store:"),
                    term::cyan(&s.base().display().to_string())
                );
                println!("store paths:");
                for p in s.paths().unwrap_or_default() {
                    println!("  {}", term::dim(&p.to_string()));
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{}", term::error(&e.to_string()));
                ExitCode::FAILURE
            }
        },
        "list" => {
            let store = match store::Store::open() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("{}", term::error(&e.to_string()));
                    return ExitCode::FAILURE;
                }
            };
            let mut installed = resolve::installed_packages(&store);
            installed.sort_by(|a, b| {
                a.meta
                    .package
                    .cmp(&b.meta.package)
                    .then(a.meta.architecture.cmp(&b.meta.architecture))
            });
            if json {
                let rows: Vec<serde_json::Value> = installed
                    .iter()
                    .map(|p| {
                        serde_json::json!({
                            "name": p.meta.package,
                            "version": p.meta.version,
                            "architecture": p.meta.architecture,
                            "description": p.meta.description,
                            "depends": deb::render_depends(&p.meta.depends),
                        })
                    })
                    .collect();
                let out = serde_json::to_string(&rows).unwrap_or_else(|_| "[]".to_string());
                println!("{out}");
                ExitCode::SUCCESS
            } else {
                if installed.is_empty() {
                    println!("{}", term::dim("no packages installed"));
                }
                for p in &installed {
                    let desc = p.meta.description.lines().next().unwrap_or("");
                    println!(
                        "{} {} {}  {}",
                        term::bold_cyan(&p.meta.package),
                        term::green(&p.meta.version),
                        term::magenta(&p.meta.architecture),
                        term::dim(desc)
                    );
                }
                ExitCode::SUCCESS
            }
        }
        "add" => {
            let file = match args.get(1) {
                Some(f) => f,
                None => {
                    eprintln!("usage: univ add <file>");
                    return ExitCode::FAILURE;
                }
            };
            let bytes = match std::fs::read(file) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("{}", term::error(&e.to_string()));
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
                    println!(
                        "{} {}",
                        term::bold_green("added"),
                        term::cyan(&s.base().join(p.to_string()).display().to_string())
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("{}", term::error(&e.to_string()));
                    ExitCode::FAILURE
                }
            }
        }
        "add-repo" => {
            let rest: Vec<&str> = args[1..].iter().map(String::as_str).collect();
            if rest.len() < 2 {
                eprintln!("usage: univ add-repo <name> <base-url> [arch ...]");
                return ExitCode::FAILURE;
            }
            let (name, base, arches) = parse_repo_args(&rest);
            match repo::add_repo(&name, &base, &arches) {
                Ok(()) => {
                    println!(
                        "{} {} {} {}",
                        term::bold_green("added"),
                        term::bold("deb repo"),
                        term::bold_cyan(&format!("'{name}'")),
                        term::dim(&format!("-> {base}"))
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("{}", term::error(&e.to_string()));
                    ExitCode::FAILURE
                }
            }
        }
        "add-rpm-repo" => {
            let rest: Vec<&str> = args[1..].iter().map(String::as_str).collect();
            if rest.len() < 2 {
                eprintln!("usage: univ add-rpm-repo <name> <base-url> [arch ...]");
                return ExitCode::FAILURE;
            }
            let (name, base, arches) = parse_repo_args(&rest);
            match rpmrepo::add_repo(&name, &base, &arches) {
                Ok(()) => {
                    println!(
                        "{} {} {} {}",
                        term::bold_green("added"),
                        term::bold("rpm repo"),
                        term::bold_cyan(&format!("'{name}'")),
                        term::dim(&format!("-> {base}"))
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("{}", term::error(&e.to_string()));
                    ExitCode::FAILURE
                }
            }
        }
        "update" => {
            let repos = match repo::repos() {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("{}", term::error(&e.to_string()));
                    return ExitCode::FAILURE;
                }
            };
            for r in &repos {
                if let Err(e) = repo::update(r) {
                    eprintln!("{}", term::error(&e.to_string()));
                    return ExitCode::FAILURE;
                }
            }
            ExitCode::SUCCESS
        }
        "install" => {
            let arg = match args.get(1) {
                Some(f) => f,
                None => {
                    eprintln!("usage: univ install <file.deb | file.rpm | package>");
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
                    eprintln!("usage: univ install-deb <package>");
                    return ExitCode::FAILURE;
                }
            };
            install_package(arg)
        }
        "install-rpm" => {
            let arg = match args.get(1) {
                Some(f) => f,
                None => {
                    eprintln!("usage: univ install-rpm <package | file.rpm>");
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
                    eprintln!("{}", term::error(&e.to_string()));
                    return ExitCode::FAILURE;
                }
            };
            for r in &repos {
                if let Err(e) = rpmrepo::update(r) {
                    eprintln!("{}", term::error(&e.to_string()));
                    return ExitCode::FAILURE;
                }
            }
            ExitCode::SUCCESS
        }
        "search" => {
            let query = match args.get(1) {
                Some(q) => q,
                None => {
                    eprintln!("usage: univ search <query>");
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
                            repo: r.name.clone(),
                            description: p.description,
                            depends: deb::render_depends(&p.depends),
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
                            repo: r.name.clone(),
                            description: p.description,
                            depends: rpm::render_requires(&p.requires),
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
                println!("{}", term::yellow(&format!("no packages match '{query}'")));
                return ExitCode::FAILURE;
            }
            let shown = results.len().min(100);
            if json {
                let rows: Vec<serde_json::Value> = results[..shown]
                    .iter()
                    .map(|p| {
                        serde_json::json!({
                            "name": p.package,
                            "version": p.version,
                            "architecture": p.architecture,
                            "kind": p.kind,
                            "repo": p.repo,
                            "description": p.description,
                            "depends": p.depends,
                        })
                    })
                    .collect();
                let out = serde_json::to_string(&rows).unwrap_or_else(|_| "[]".to_string());
                println!("{out}");
                return ExitCode::SUCCESS;
            }
            for p in &results[..shown] {
                let desc_part = if p.description.is_empty() {
                    String::new()
                } else {
                    format!(" - {}", term::dim(&p.description))
                };
                let pkg = if p.kind == "rpm" {
                    term::yellow(&p.package)
                } else {
                    term::bold_cyan(&p.package)
                };
                println!(
                    "{pkg} {} [{}] {}{desc_part}",
                    term::green(&p.version),
                    term::magenta(&p.architecture),
                    term::magenta(p.kind),
                );
            }
            if results.len() > shown {
                println!(
                    "{}",
                    term::dim(&format!("... and {} more", results.len() - shown))
                );
            }
            ExitCode::SUCCESS
        }
        "deps" => {
            let package = match args.get(1) {
                Some(p) => p,
                None => {
                    eprintln!("usage: univ deps <package>");
                    return ExitCode::FAILURE;
                }
            };
            let store = match store::Store::open() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("{}", term::error(&e.to_string()));
                    return ExitCode::FAILURE;
                }
            };
            let installed = resolve::installed_packages(&store);
            let pkg = match find_installed(&installed, package) {
                Some(p) => p,
                None => {
                    eprintln!(
                        "{}",
                        term::error(&format!("no installed package matching '{package}'"))
                    );
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
                    let status = if nspawn::on_filesystem(std::path::Path::new(&interp)) {
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
                    eprintln!("{}", term::error(&e.to_string()));
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
                        println!(
                            "{} {}",
                            term::bold_green("relinked"),
                            term::bold_cyan(&pkg.meta.package)
                        );
                    }
                    Err(e) => eprintln!(
                        "{}",
                        term::warn(&format!("warning: relink {}: {e}", pkg.meta.package))
                    ),
                }
            }
            println!("{}", term::bold(&format!("rehashed {relinked} package(s)")));
            ExitCode::SUCCESS
        }
        "lock" => {
            let lock = lock::Lockfile::load();
            if json {
                let rows: Vec<serde_json::Value> = lock
                    .entries()
                    .iter()
                    .map(|e| {
                        serde_json::json!({
                            "name": e.package,
                            "version": e.version,
                            "architecture": e.architecture,
                            "sha256": e.sha256,
                            "repo": e.base,
                            "kind": e.kind,
                        })
                    })
                    .collect();
                let out = serde_json::to_string(&rows).unwrap_or_else(|_| "[]".to_string());
                println!("{out}");
            } else if lock.is_empty() {
                println!("{}", term::dim("no packages locked"));
            } else {
                for e in lock.entries() {
                    let short = &e.sha256[..e.sha256.len().min(16)];
                    println!(
                        "{} {}-{} [{}] {}  {}  {}",
                        term::bold_cyan(&e.package),
                        term::green(&e.version),
                        term::magenta(&e.architecture),
                        term::magenta(&e.kind),
                        term::dim(&e.base),
                        term::dim(short),
                        term::dim("(lock.json)")
                    );
                }
            }
            ExitCode::SUCCESS
        }
        "upgrade" => {
            let store = match store::Store::open() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("{}", term::error(&e.to_string()));
                    return ExitCode::FAILURE;
                }
            };
            let mut upgraded = 0;
            let mut failed = false;
            if let Ok(repos) = repo::repos() {
                for r in &repos {
                    match repo::upgrade(&store, r) {
                        Ok(installed) => {
                            upgraded += report_upgrades(&store, &installed);
                        }
                        Err(e) => {
                            eprintln!("{}", term::error(&e.to_string()));
                            failed = true;
                        }
                    }
                }
            }
            if let Ok(repos) = rpmrepo::repos() {
                for r in &repos {
                    match rpmrepo::upgrade(&store, r) {
                        Ok(installed) => {
                            upgraded += report_upgrades(&store, &installed);
                        }
                        Err(e) => {
                            eprintln!("{}", term::error(&e.to_string()));
                            failed = true;
                        }
                    }
                }
            }
            if upgraded == 0 && !failed {
                println!("{}", term::dim("nothing to upgrade"));
            }
            if failed && upgraded == 0 {
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        "autoclean" => match link::autoclean() {
            Ok(orphans) => {
                if orphans.is_empty() {
                    println!("{}", term::dim("nothing to clean"));
                } else {
                    println!(
                        "{} {} ({})",
                        term::bold_green("removed"),
                        orphans.join(", "),
                        term::dim(&format!("{} orphaned", orphans.len()))
                    );
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{}", term::error(&e.to_string()));
                ExitCode::FAILURE
            }
        },
        "uninstall" => {
            let package = match args.get(1) {
                Some(p) => p,
                None => {
                    eprintln!("usage: univ uninstall <package>");
                    return ExitCode::FAILURE;
                }
            };
            match link::uninstall(package) {
                Ok((links, paths, orphans)) => {
                    println!(
                        "{} {links} link(s) and {paths} store path(s) for {}",
                        term::bold_green("removed"),
                        term::bold_cyan(&format!("'{package}'"))
                    );
                    if !orphans.is_empty() {
                        println!(
                            "{} {}",
                            term::yellow("removed orphaned dependencies:"),
                            orphans.join(", ")
                        );
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("{}", term::error(&e.to_string()));
                    ExitCode::FAILURE
                }
            }
        }
        "unlink" => {
            let package = match args.get(1) {
                Some(p) => p,
                None => {
                    eprintln!("usage: univ unlink <package>");
                    return ExitCode::FAILURE;
                }
            };
            match store::Store::open().and_then(|_s| link::unlink(package)) {
                Ok(n) => {
                    println!(
                        "{} {n} link(s) for {}",
                        term::bold_green("removed"),
                        term::bold_cyan(&format!("'{package}'"))
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("{}", term::error(&e.to_string()));
                    ExitCode::FAILURE
                }
            }
        }
        "profile" => run_profile(&args[1..]),
        other => {
            eprintln!("{}", term::error(&format!("unknown command '{other}'")));
            ExitCode::FAILURE
        }
    }
}

fn ensure_crun_runtime() -> io::Result<()> {
    nspawn::init(nspawn::ContainerKind::Deb)?;
    nspawn::init(nspawn::ContainerKind::Rpm)?;
    nspawn::bootstrap()?;
    Ok(())
}

fn run_store() -> ExitCode {
    let dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let bin = if dir.join("univ-store").is_file() {
        dir.join("univ-store")
    } else {
        std::path::PathBuf::from("univ-store")
    };
    match std::process::Command::new(bin).status() {
        Ok(status) if status.success() => ExitCode::SUCCESS,
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(e) => {
            eprintln!(
                "{}",
                term::error(&format!("failed to launch the store TUI: {e}"))
            );
            eprintln!(
                "{}",
                term::dim("hint: build it with `cargo build` (produces the `univ-store` binary)")
            );
            ExitCode::FAILURE
        }
    }
}

fn install_package(arg: &str) -> ExitCode {
    if let Err(e) = ensure_crun_runtime() {
        eprintln!(
            "{}",
            term::warn(&format!("crun runtime setup incomplete: {e}"))
        );
    }
    let store = match store::Store::open() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{}", term::error(&e.to_string()));
            return ExitCode::FAILURE;
        }
    };
    let repo = match repo::repos().and_then(|r| {
        r.first()
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no repositories configured"))
    }) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{}", term::error(&e.to_string()));
            return ExitCode::FAILURE;
        }
    };
    match repo::install(&store, &repo, arg) {
        Ok(installed) => {
            for (sp, meta) in &installed {
                println!("{}", installed_line(meta));
                let _ = link::link_package(&store, sp, meta);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{}", term::error(&e.to_string()));
            ExitCode::FAILURE
        }
    }
}

fn report_upgrades(store: &store::Store, installed: &[(store::StorePath, deb::DebMeta)]) -> usize {
    for (sp, meta) in installed {
        println!(
            "{} {}-{} [{}]",
            term::bold_green("upgraded"),
            term::bold_cyan(&meta.package),
            term::green(&meta.version),
            term::magenta(&meta.architecture)
        );
        if let Err(e) = link::link_package(store, sp, meta) {
            eprintln!(
                "{}",
                term::warn(&format!("warning: relink {}: {e}", meta.package))
            );
        }
    }
    installed.len()
}

fn installed_line(meta: &deb::DebMeta) -> String {
    format!(
        "{} {}-{} [{}]",
        term::bold_green("installed"),
        term::bold_cyan(&meta.package),
        term::green(&meta.version),
        term::magenta(&meta.architecture)
    )
}

fn print_help() {
    println!("univ - a content-addressed package manager");
    println!();
    println!("packages are installed into a crun-backed container root (~/.local/univ/container)");
    println!("and run via crun launchers in ~/.local/bin, keeping the host filesystem clean");
    println!();
    println!("usage: univ <command> [args]");
    println!();
    println!("commands:");
    println!(
        "  init                    create the store at ~/.local/univ (with default deb & rpm repos)"
    );
    println!(
        "  bootstrap               install a base OS into the container (dnf --installroot / debootstrap; needs root)"
    );
    println!(
        "  apply [file.kdl]        apply a declarative KDL config (default ~/.local/univ/config.kdl)"
    );
    println!("  store (or --store)      open the interactive store TUI (univ-store)");
    println!("  status                  list installed store paths");
    println!("  list                    list installed packages (add --json for machine-readable)");
    println!();
    println!("  -- Debian / Ubuntu (APT/deb) --");
    println!("  update                  refresh the deb package index from repos");
    println!("  add-repo <n> <url> [a…] append a deb repo to ~/.local/univ/debrepos.conf");
    println!("  search <query>          search cached package indexes (deb & rpm); add --json");
    println!("  install <file.deb>      install a .deb from disk");
    println!("  install <package>       install a deb package (with deps) from a repo");
    println!("  install-deb <package>   install a deb package by name");
    println!();
    println!("  -- Fedora / RPM (DNF/rpm) --");
    println!("  update-rpm              refresh the RPM package index from repos");
    println!("  add-rpm-repo <n> <url>  append an RPM repo to ~/.local/univ/rpmrepos.conf");
    println!("  install <file.rpm>      install a .rpm from disk");
    println!("  install-rpm <package>   install an RPM package (with deps) from a repo");
    println!("  install-rpm <file.rpm>  install a .rpm from disk");
    println!();
    println!("  -- General --");
    println!("  deps <package>          show how a package's shared libraries resolve");
    println!("  rehash                  rebuild launchers for all installed packages");
    println!("  unlink <package>        remove a package's launchers and desktop entries");
    println!("  uninstall <package>     remove a package's files, launchers and store path");
    println!("                          (plus no-longer-needed dependencies)");
    println!(
        "  upgrade                 upgrade installed packages to the newest available version"
    );
    println!("                          (downloads only the changed packages)");
    println!("  autoclean               remove orphaned dependency packages");
    println!("  lock                    show the pinned package versions (lock.json)");
    println!();
    println!("  -- Profiles --");
    println!("  profile new <name> <pkg...>   save a declarative profile listing packages");
    println!("  profile add <name> <pkg...>   append packages to a profile");
    println!("  profile list                  list saved profiles");
    println!("  profile show <name>           print a profile's packages");
    println!("  profile rm <name>             delete a profile");
    println!("  profile apply <name>          sync the store to a profile in one transaction");
    println!("                                (installs missing packages, removes extras)");
}

fn run_profile(args: &[String]) -> ExitCode {
    let sub = match args.first() {
        Some(s) => s.as_str(),
        None => {
            eprintln!("usage: univ profile <new|list|show|rm|apply> ...");
            return ExitCode::FAILURE;
        }
    };
    match sub {
        "new" => {
            let Some((name, pkgs)) = split_profile_args(&args[1..]) else {
                eprintln!("usage: univ profile new <name> <package ...>");
                return ExitCode::FAILURE;
            };
            match profile::save(&name, &pkgs) {
                Ok(()) => {
                    warn_unknown(&pkgs);
                    println!(
                        "{} {} ({})",
                        term::bold_green("saved"),
                        term::bold_cyan(&name),
                        term::dim(&format!("{} package(s)", pkgs.len()))
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("{}", term::error(&e.to_string()));
                    ExitCode::FAILURE
                }
            }
        }
        "add" => {
            let Some((name, pkgs)) = split_profile_args(&args[1..]) else {
                eprintln!("usage: univ profile add <name> <package ...>");
                return ExitCode::FAILURE;
            };
            let mut current = profile::show(&name).unwrap_or_default();
            for p in pkgs {
                if !current.contains(&p) {
                    current.push(p);
                }
            }
            match profile::save(&name, &current) {
                Ok(()) => {
                    warn_unknown(&current);
                    println!(
                        "{} {} ({})",
                        term::bold_green("saved"),
                        term::bold_cyan(&name),
                        term::dim(&format!("{} package(s)", current.len()))
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("{}", term::error(&e.to_string()));
                    ExitCode::FAILURE
                }
            }
        }
        "list" => match profile::list() {
            Ok(names) => {
                if names.is_empty() {
                    println!("{}", term::dim("no profiles"));
                }
                for n in names {
                    println!("{}", term::bold_cyan(&n));
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{}", term::error(&e.to_string()));
                ExitCode::FAILURE
            }
        },
        "show" => {
            let Some(name) = args.get(1) else {
                eprintln!("usage: univ profile show <name>");
                return ExitCode::FAILURE;
            };
            match profile::show(name) {
                Ok(pkgs) => {
                    for p in pkgs {
                        println!("{}", p);
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("{}", term::error(&e.to_string()));
                    ExitCode::FAILURE
                }
            }
        }
        "rm" | "remove" => {
            let Some(name) = args.get(1) else {
                eprintln!("usage: univ profile rm <name>");
                return ExitCode::FAILURE;
            };
            match profile::remove(name) {
                Ok(()) => {
                    println!(
                        "{} {}",
                        term::bold_green("removed"),
                        term::bold_cyan(&format!("'{}'", name))
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("{}", term::error(&e.to_string()));
                    ExitCode::FAILURE
                }
            }
        }
        "apply" => {
            let Some(name) = args.get(1) else {
                eprintln!("usage: univ profile apply <name>");
                return ExitCode::FAILURE;
            };
            let store = match store::Store::open() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("{}", term::error(&e.to_string()));
                    return ExitCode::FAILURE;
                }
            };
            match profile::apply(&store, name) {
                Ok(res) => {
                    if res.installed == 0 && res.removed == 0 {
                        println!("{}", term::dim("already in sync"));
                    }
                    if res.installed > 0 {
                        println!(
                            "{} {} package(s)",
                            term::bold_green("installed"),
                            res.installed
                        );
                    }
                    if res.removed > 0 {
                        println!("{} {} package(s)", term::bold_green("removed"), res.removed);
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("{}", term::error(&e.to_string()));
                    ExitCode::FAILURE
                }
            }
        }
        other => {
            eprintln!(
                "{}",
                term::error(&format!("unknown profile command '{other}'"))
            );
            ExitCode::FAILURE
        }
    }
}

fn split_profile_args(rest: &[String]) -> Option<(String, Vec<String>)> {
    if rest.is_empty() {
        return None;
    }
    let name = rest[0].clone();
    let pkgs: Vec<String> = rest[1..]
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    Some((name, pkgs))
}

fn warn_unknown(pkgs: &[String]) {
    let mut available: std::collections::HashSet<String> =
        repo::available_names().into_iter().collect();
    available.extend(rpmrepo::available_names());
    for p in pkgs {
        if !available.contains(p) {
            eprintln!(
                "{}",
                term::warn(&format!(
                    "'{p}' is not in any repo index (run `univ update` / `univ update-rpm`)"
                ))
            );
        }
    }
}

fn report_linked(linked: &link::Linked) {
    if !linked.bin_links.is_empty() {
        println!("{}", term::bold("linked into ~/.local/bin:"));
        for b in &linked.bin_links {
            println!("  {}", term::cyan(&b.display().to_string()));
        }
        let bin_dir = store::Store::home_dir()
            .map(|h| h.join(".local").join("bin"))
            .unwrap_or_default();
        if !on_path(&bin_dir) {
            println!(
                "{}",
                term::yellow(&format!(
                    "note: {} is not on $PATH; add it to run these from a terminal",
                    bin_dir.display()
                ))
            );
        }
    }
    if !linked.desktop_files.is_empty() {
        println!("{}", term::bold("desktop launchers:"));
        for d in &linked.desktop_files {
            println!("  {}", term::cyan(&d.display().to_string()));
        }
    }
    if !linked.icons.is_empty() {
        println!(
            "{} {}",
            term::bold("icons:"),
            term::cyan(&linked.icons.len().to_string())
        );
    }
}

fn install_deb_file(file: &str) -> ExitCode {
    if let Err(e) = ensure_crun_runtime() {
        eprintln!(
            "{}",
            term::warn(&format!("crun runtime setup incomplete: {e}"))
        );
    }
    let bytes = match std::fs::read(file) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{}", term::error(&e.to_string()));
            return ExitCode::FAILURE;
        }
    };
    match store::Store::open().and_then(|s| {
        let (p, meta) = deb::install(&s, &bytes)?;
        deb::write_meta(&meta, &p)?;
        store::mark_manual(&p)?;
        let mut lock = lock::Lockfile::load();
        lock.set(lock::LockEntry {
            package: meta.package.clone(),
            version: meta.version.clone(),
            architecture: meta.architecture.clone(),
            sha256: store::sha256_hex(&bytes),
            base: format!("file:{file}"),
            kind: "deb".to_string(),
        });
        lock.save()?;
        Ok((s, p, meta))
    }) {
        Ok((s, p, meta)) => {
            println!("{}", installed_line(&meta));
            println!(
                "{} {}",
                term::bold("store:"),
                term::cyan(&s.base().join(p.to_string()).display().to_string())
            );
            if !meta.description.is_empty() {
                let first = meta.description.lines().next().unwrap_or("");
                println!("  {}", term::dim(first));
            }
            if !meta.depends.is_empty() {
                println!(
                    "  {} {}",
                    term::bold("depends:"),
                    term::dim(&deb::render_depends(&meta.depends))
                );
            }
            match link::link_package(&s, &p, &meta) {
                Ok(linked) => report_linked(&linked),
                Err(e) => eprintln!(
                    "{}",
                    term::warn(&format!("warning: integration failed: {e}"))
                ),
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{}", term::error(&e.to_string()));
            ExitCode::FAILURE
        }
    }
}

fn install_rpm_file(file: &str) -> ExitCode {
    if let Err(e) = ensure_crun_runtime() {
        eprintln!(
            "{}",
            term::warn(&format!("crun runtime setup incomplete: {e}"))
        );
    }
    let bytes = match std::fs::read(file) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{}", term::error(&e.to_string()));
            return ExitCode::FAILURE;
        }
    };
    match store::Store::open().and_then(|s| {
        let (p, rpm_meta) = rpm::install(&s, &bytes)?;
        rpm::write_meta(&rpm_meta, &p)?;
        let meta: deb::DebMeta = rpm_meta.into();
        store::mark_manual(&p)?;
        let mut lock = lock::Lockfile::load();
        lock.set(lock::LockEntry {
            package: meta.package.clone(),
            version: meta.version.clone(),
            architecture: meta.architecture.clone(),
            sha256: store::sha256_hex(&bytes),
            base: format!("file:{file}"),
            kind: "rpm".to_string(),
        });
        lock.save()?;
        Ok((s, p, meta))
    }) {
        Ok((s, p, meta)) => {
            println!("{}", installed_line(&meta));
            println!(
                "{} {}",
                term::bold("store:"),
                term::cyan(&s.base().join(p.to_string()).display().to_string())
            );
            if !meta.description.is_empty() {
                let first = meta.description.lines().next().unwrap_or("");
                println!("  {}", term::dim(first));
            }
            match link::link_package(&s, &p, &meta) {
                Ok(linked) => report_linked(&linked),
                Err(e) => eprintln!(
                    "{}",
                    term::warn(&format!("warning: integration failed: {e}"))
                ),
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{}", term::error(&e.to_string()));
            ExitCode::FAILURE
        }
    }
}

fn install_rpm_package(name: &str) -> ExitCode {
    if let Err(e) = ensure_crun_runtime() {
        eprintln!(
            "{}",
            term::warn(&format!("crun runtime setup incomplete: {e}"))
        );
    }
    let store = match store::Store::open() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{}", term::error(&e.to_string()));
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
            eprintln!("{}", term::error(&e.to_string()));
            return ExitCode::FAILURE;
        }
    };
    match rpmrepo::install(&store, &repo, name) {
        Ok(installed) => {
            for (sp, meta) in &installed {
                println!("{}", installed_line(meta));
                let _ = link::link_package(&store, sp, meta);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{}", term::error(&e.to_string()));
            ExitCode::FAILURE
        }
    }
}

fn find_installed<'a>(
    installed: &'a [resolve::Installed],
    name: &str,
) -> Option<&'a resolve::Installed> {
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
            installed
                .iter()
                .find(|p| p.sp.name() == name || p.sp.name().starts_with(&format!("{name}-")))
        })
}

fn on_path(dir: &std::path::Path) -> bool {
    std::env::var("PATH")
        .map(|p| {
            p.split(':')
                .any(|d| !d.is_empty() && std::path::Path::new(d) == dir)
        })
        .unwrap_or(false)
}

fn parse_repo_args(rest: &[&str]) -> (String, String, Vec<String>) {
    let name = rest[0].to_string();
    let base = rest[1].to_string();
    let arches: Vec<String> = rest[2..].iter().map(|s| s.to_string()).collect();
    (name, base, arches)
}

const BASHRC_START: &str = "# >>> univ >>>";
const BASHRC_END: &str = "# <<< univ <<<";

fn ensure_bashrc() -> io::Result<()> {
    let home = store::Store::home_dir().unwrap_or_default();
    let rc = home.join(".bashrc");
    let existing = std::fs::read_to_string(&rc).unwrap_or_default();
    if existing.contains(BASHRC_START) && existing.contains(BASHRC_END) {
        return Ok(());
    }
    let bin_dir = home.join(".local/bin");
    let mut text = existing;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!(
        "{BASHRC_START}\n\
         # managed by univ init (see `univ --help`)\n\
         export PATH=\"{}:${{PATH:+:$PATH}}\"\n\
         {BASHRC_END}\n",
        bin_dir.display()
    ));
    std::fs::write(&rc, text)?;
    println!(
        "{} {}",
        term::bold_green("wrote"),
        term::cyan(&rc.display().to_string())
    );
    Ok(())
}
