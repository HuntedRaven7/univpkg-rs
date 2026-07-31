# unipkg

## How it works

- **Store**: every installed package's files live under a content-addressed
  directory `~/.local/unipkg/store/<sha256>-<name>/`, computed by hashing the
  package tree. Re-installing identical content is a no-op.
- **Package formats**: `.deb` archives (ar + tar control/data members) and
  `.rpm` archives (lead + signature + header + payload) are parsed directly,
  with no dependency on `dpkg` or `rpm` being installed.
- **Repositories**: APT-style repos (`Packages.gz`/`Release`) and DNF-style
  repos (`repomd.xml` + `primary.xml`) are fetched and cached so you can
  search and install packages by name, with dependencies pulled in
  automatically and version constraints checked using Debian's version
  comparison rules.
- **Linking**: after install, executables are symlinked into `~/.local/bin`,
  and any bundled `.desktop` files and icons are copied into
  `~/.local/share/applications`, `~/.local/share/icons`, and
  `~/.local/share/pixmaps` so installed apps show up in a normal desktop
  environment.

## Goals

core
  - dependency resolution (with conflict detection)
  - reproducible/deterministic installs (lockfile)
  - transactional installs (atomic, rollback on failure)
  - signature verification for packages/repos

quality of life
  - parallel downloads
  - delta updates (only fetch diffs)
  - search with fuzzy matching
  - clean orphan/unused dependency removal

advanced
  - sandboxed builds (containers or namespaces)
  - multiple repo/mirror support with priority
  - binary + source package support
  - hooks (pre/post install scripts)
  - offline/local repo caching

nice-to-have
  - a TUI for browsing/searching packages
  - integration with existing formats (so it's not a total island)

## Usage

```
unipkg init                      create the store at ~/.local/unipkg

unipkg status                    list installed store paths

# Debian / Ubuntu (APT/deb)
unipkg update                    refresh the deb package index from repos
unipkg add-repo <name> <url> [arch...]   append a deb repo
unipkg search <query>            search cached package indexes (deb & rpm)
unipkg install <file.deb>        install a .deb from disk
unipkg install <package>         install a deb package (with deps) from a repo
unipkg install-deb <package>     install a deb package by name

# Fedora / RPM (DNF/rpm)
unipkg update-rpm                refresh the RPM package index from repos
unipkg add-rpm-repo <name> <url> append an RPM repo
unipkg install <file.rpm>        install a .rpm from disk
unipkg install-rpm <package>     install an RPM package (with deps) from a repo
unipkg install-rpm <file.rpm>    install a .rpm from disk

# General
unipkg deps <package>            show how a package's shared libraries resolve
unipkg rehash                    rebuild launchers for all installed packages
unipkg unlink <package>          remove a package's launchers and desktop entries
unipkg uninstall <package>       remove a package's files, launchers and store path
```

## Project layout

| File | Responsibility |
| --- | --- |
| `main.rs` | CLI entry point and command dispatch |
| `store.rs` | content-addressed store, plus a self-contained SHA-256 implementation |
| `deb.rs` | `.deb` archive parsing, control-file metadata, and install logic |
| `rpm.rs` | `.rpm` archive parsing (lead/header/payload), metadata, and install logic |
| `repo.rs` | APT repository index fetching, caching, search, and dependency install |
| `rpmrepo.rs` | DNF/Fedora repository (`repomd.xml`/`primary.xml`) fetching and install |
| `elf.rs` | minimal ELF parser used to read `DT_NEEDED` entries and the interpreter |
| `resolve.rs` | resolves a binary's shared library dependencies against the store and system |
| `link.rs` | symlinks binaries and installs desktop entries/icons into `~/.local` |
| `version.rs` | Debian-style version comparison (`dpkg --compare-versions` semantics) |

## Building

```
cargo build --release
```

## Testing

```
cargo test
```
