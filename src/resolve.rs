use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::deb::{self, DebMeta};
use crate::elf;
use crate::store::{Store, StorePath};

const SYSTEM_LIB_DIRS: &[&str] = &[
    "/lib",
    "/usr/lib",
    "/lib64",
    "/usr/lib64",
    "/lib/x86_64-linux-gnu",
    "/usr/lib/x86_64-linux-gnu",
    "/lib/i386-linux-gnu",
    "/usr/lib/i386-linux-gnu",
    "/lib/aarch64-linux-gnu",
    "/usr/lib/aarch64-linux-gnu",
    "/usr/local/lib",
    "/usr/local/lib64",
];

const STORE_LIB_DIRS: &[&str] = &[
    "usr/lib/x86_64-linux-gnu",
    "usr/lib/i386-linux-gnu",
    "usr/lib/aarch64-linux-gnu",
    "usr/lib",
    "usr/lib64",
    "usr/local/lib",
    "lib/x86_64-linux-gnu",
    "lib/i386-linux-gnu",
    "lib/aarch64-linux-gnu",
    "lib",
    "lib64",
];

pub struct Installed {
    pub sp: StorePath,
    pub meta: DebMeta,
    pub root: PathBuf,
}

pub fn installed_packages(store: &Store) -> Vec<Installed> {
    store
        .paths()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|sp| {
            let meta = deb::read_meta(&sp).ok()?;
            let root = store.base().join(sp.to_string());
            Some(Installed { sp, meta, root })
        })
        .collect()
}

pub fn system_library(soname: &str, class: u8, machine: u16) -> Option<PathBuf> {
    SYSTEM_LIB_DIRS
        .iter()
        .map(Path::new)
        .map(|d| d.join(soname))
        .find(|p| p.exists() && arch_matches(p, class, machine))
}

fn store_library(pkg: &Installed, soname: &str, class: u8, machine: u16) -> Option<PathBuf> {
    for rel in STORE_LIB_DIRS {
        let dir = pkg.root.join(rel);
        let file = dir.join(soname);
        if file.exists() && arch_matches(&file, class, machine) {
            return Some(dir);
        }
    }
    None
}

fn arch_matches(file: &Path, class: u8, machine: u16) -> bool {
    match elf::read_elf_file(file).ok().flatten() {
        Some(info) => info.class == class && info.machine == machine,
        None => true,
    }
}

#[derive(Clone, Debug)]
pub enum LibSource {
    System(PathBuf),
    Store { package: String, dir: PathBuf },
    Missing,
}

#[derive(Clone, Debug)]
pub struct ResolvedLib {
    pub name: String,
    pub source: LibSource,
}

pub fn resolve_binary(binary: &Path, installed: &[Installed]) -> Vec<ResolvedLib> {
    let mut resolved: Vec<ResolvedLib> = Vec::new();
    let mut handled: BTreeSet<PathBuf> = BTreeSet::new();
    let mut queue = vec![binary.to_path_buf()];

    let Some(first) = elf::read_elf_file(binary).ok().flatten() else {
        return resolved;
    };
    let (class, machine) = (first.class, first.machine);

    while let Some(file) = queue.pop() {
        if !handled.insert(file.clone()) {
            continue;
        }
        let Some(info) = elf::read_elf_file(&file).ok().flatten() else {
            continue;
        };
        if !info.is_executable() {
            continue;
        }
        for lib in info.needed {
            if resolved.iter().any(|r: &ResolvedLib| r.name == lib) {
                continue;
            }
            let from_store = installed.iter().find_map(|pkg| {
                store_library(pkg, &lib, class, machine).map(|dir| (pkg, dir))
            });
            let source = match from_store {
                Some((pkg, dir)) => {
                    queue.push(dir.join(&lib));
                    LibSource::Store {
                        package: pkg.meta.package.clone(),
                        dir,
                    }
                }
                None => match system_library(&lib, class, machine) {
                    Some(path) => LibSource::System(path),
                    None => LibSource::Missing,
                },
            };
            resolved.push(ResolvedLib { name: lib, source });
        }
    }

    resolved.sort_by(|a, b| a.name.cmp(&b.name));
    resolved
}

pub fn store_lib_dirs(deps: &[ResolvedLib]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for d in deps {
        if let LibSource::Store { dir, .. } = &d.source {
            if !out.contains(dir) {
                out.push(dir.clone());
            }
        }
    }
    out
}

pub fn interpreter(binary: &Path) -> Option<String> {
    elf::read_elf_file(binary)
        .ok()
        .flatten()
        .and_then(|i| i.interpreter)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_store;
    use std::fs;

    fn write_bin(
        store: &Store,
        name: &str,
        needed: &[u32],
        strtab: &[u8],
    ) -> std::path::PathBuf {
        let sp = store
            .add_tree(name, |dir, _ctx| {
                let bytes = crate::elf::tests::build_dyn(
                    strtab,
                    needed,
                    Some("/lib64/ld-linux-x86-64.so.2"),
                );
                fs::create_dir_all(dir.join("usr/bin"))?;
                fs::write(dir.join("usr/bin/foo"), &bytes)?;
                fs::create_dir_all(dir.join("usr/lib/x86_64-linux-gnu"))?;
                Ok(())
            })
            .unwrap();
        store.base().join(sp.to_string())
    }

    fn meta(package: &str) -> DebMeta {
        DebMeta {
            package: package.into(),
            version: "1.0".into(),
            architecture: "amd64".into(),
            ..Default::default()
        }
    }

    fn installed(store: &Store, sp: &crate::store::StorePath, package: &str) -> Installed {
        Installed {
            sp: sp.clone(),
            meta: meta(package),
            root: store.base().join(sp.to_string()),
        }
    }

    #[test]
    fn resolves_against_installed_store_package() {
        let tmp = std::env::temp_dir().join(format!(
            "unipkg-resolve-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let store = test_store(&tmp.join("store"));
        fs::create_dir_all(store.base()).unwrap();

        let libfoo = b"\0libfoo.so.1\0";
        let app_root = write_bin(&store, "app", &[1], libfoo);
        let app_sp = crate::store::StorePath::parse(
            app_root.file_name().unwrap().to_str().unwrap(),
        )
        .unwrap();
        let app = installed(&store, &app_sp, "app");

        let lib_sp = store
            .add_tree("libfoo", |dir, _ctx| {
                fs::create_dir_all(dir.join("usr/lib/x86_64-linux-gnu"))?;
                fs::write(
                    dir.join("usr/lib/x86_64-linux-gnu/libfoo.so.1"),
                    b"fake lib",
                )?;
                Ok(())
            })
            .unwrap();
        let lib_root = store.base().join(lib_sp.to_string());
        let libfoo_pkg = installed(&store, &lib_sp, "libfoo");

        let deps = resolve_binary(&app_root.join("usr/bin/foo"), &[libfoo_pkg, app]);
        assert_eq!(deps.len(), 1);
        let d = &deps[0];
        assert_eq!(d.name, "libfoo.so.1");
        let LibSource::Store { package, dir } = &d.source else {
            panic!("expected store source, got {:?}", d.source);
        };
        assert_eq!(package, "libfoo");
        assert_eq!(dir, &lib_root.join("usr/lib/x86_64-linux-gnu"));
        assert_eq!(store_lib_dirs(&deps), vec![dir.clone()]);

        fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn amd64_binary_ignores_i386_lib_with_same_soname() {
        let tmp = std::env::temp_dir().join(format!(
            "unipkg-resolve-arch-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let store = test_store(&tmp.join("store"));
        fs::create_dir_all(store.base()).unwrap();

        let libfoo = b"\0libfoo.so.1\0";
        let app_root = write_bin(&store, "app", &[1], libfoo);
        let app_sp = crate::store::StorePath::parse(
            app_root.file_name().unwrap().to_str().unwrap(),
        )
        .unwrap();
        let _app = installed(&store, &app_sp, "app");

        let lib64_sp = store
            .add_tree("libfoo-amd64", |dir, _ctx| {
                fs::create_dir_all(dir.join("usr/lib/x86_64-linux-gnu"))?;
                fs::write(
                    dir.join("usr/lib/x86_64-linux-gnu/libfoo.so.1"),
                    crate::elf::tests::build_dyn(
                        libfoo,
                        &[],
                        Some("/lib64/ld-linux-x86-64.so.2"),
                    ),
                )?;
                Ok(())
            })
            .unwrap();
        let lib32_sp = store
            .add_tree("libfoo-i386", |dir, _ctx| {
                fs::create_dir_all(dir.join("usr/lib/i386-linux-gnu"))?;
                fs::write(
                    dir.join("usr/lib/i386-linux-gnu/libfoo.so.1"),
                    crate::elf::tests::build_dyn_i386(libfoo, &[]),
                )?;
                Ok(())
            })
            .unwrap();
        let pkg64 = installed(&store, &lib64_sp, "libfoo-amd64");
        let pkg32 = installed(&store, &lib32_sp, "libfoo-i386");

        let deps = resolve_binary(&app_root.join("usr/bin/foo"), &[pkg32, pkg64]);
        assert_eq!(deps.len(), 1);
        let LibSource::Store { package, dir } = &deps[0].source else {
            panic!("expected store source, got {:?}", deps[0].source);
        };
        assert_eq!(package, "libfoo-amd64");
        assert!(dir.to_string_lossy().contains("x86_64-linux-gnu"));

        fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn missing_libs_are_reported() {
        let tmp = std::env::temp_dir().join(format!(
            "unipkg-resolve-missing-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let store = test_store(&tmp.join("store"));
        fs::create_dir_all(store.base()).unwrap();

        let strtab = b"\0libnope.so.42\0";
        let app_root = write_bin(&store, "app", &[1], strtab);
        let app_sp = crate::store::StorePath::parse(
            app_root.file_name().unwrap().to_str().unwrap(),
        )
        .unwrap();
        let app = installed(&store, &app_sp, "app");

        let deps = resolve_binary(&app_root.join("usr/bin/foo"), &[app]);
        assert!(matches!(deps[0].source, LibSource::Missing));
        assert!(store_lib_dirs(&deps).is_empty());

        fs::remove_dir_all(&tmp).unwrap();
    }
}
