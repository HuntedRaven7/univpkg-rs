# univ

## How it works

- **Store**: every installed package's files live under a content-addressed
  directory `~/.local/univ/store/<sha256>-<name>/`, computed by hashing the
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
  - integration with existing formats (so it's not a total island)

## Usage

```
univ init                      create the store at ~/.local/univ and write the
                               default Debian and Fedora repos

univ status                    list installed store paths
univ list                      list installed packages (name, version, arch, description)
univ list --json               same, as JSON for scripting/TUI use

# Debian / Ubuntu (APT/deb)
univ update                    refresh the deb package index from repos
univ add-repo <name> <url> [arch...]   append a deb repo
univ search <query>            search cached package indexes (deb & rpm)
univ search <query> --json     same, as JSON
univ install <file.deb>        install a .deb from disk
univ install <package>         install a deb package (with deps) from a repo
univ install-deb <package>     install a deb package by name

# Fedora / RPM (DNF/rpm)
univ update-rpm                refresh the RPM package index from repos
univ add-rpm-repo <name> <url> append an RPM repo
univ install <file.rpm>        install a .rpm from disk
univ install-rpm <package>     install an RPM package (with deps) from a repo
univ install-rpm <file.rpm>    install a .rpm from disk

# General
univ deps <package>            show how a package's shared libraries resolve
univ rehash                    rebuild launchers for all installed packages
univ unlink <package>          remove a package's launchers and desktop entries
univ uninstall <package>       remove a package's files, launchers and store path
                               (plus any no-longer-needed dependencies)
univ autoclean                 remove all orphaned dependency packages
```

## TUI store manager

The store is a ratatui-based TUI with a two-pane, app-store style layout: the
package list on the left feeds a live details panel on the right. Switch
between the **Installed** view (uninstall/refresh what you have) and the
**Browse** view (search the repo indexes and install), plus a full-screen
**Log** of running install/uninstall tasks. It builds as the `univ-store`
binary alongside `univ` and is launched with:

```
univ --store
```

A plain `cargo build` or `cargo install --path .` produces both `univ` and
`univ-store`, and `univ --store` runs the TUI via the sibling binary (no extra
PATH setup needed). The TUI shells out to `univ` for its data; override with
`UNIV_BIN` if you want it to use a specific build.

| Keys | Action |
| --- | --- |
| `Tab` / `1` `2` `3` | switch Installed / Browse / Log views |
| `j` / `k` | move the selection (or scroll the log) |
| `/` | focus the filter box (Installed) or search box (Browse) |
| `Enter` | install the selected search result / uninstall the selected package |
| `i` | install the selected search result (Browse) |
| `u` | uninstall the selected package (Installed) |
| `r` | refresh the installed list |
| `n` | start a new search (Browse) |
| `f` / `G` | log: toggle follow / jump to end |
| `q` / `Ctrl-C` | quit |

## Project layout

| File | Responsibility |
| --- | --- |
| `main.rs` | CLI entry point and command dispatch (`univ --store` launches the TUI) |
| `store.rs` | content-addressed store, plus a self-contained SHA-256 implementation |
| `deb.rs` | `.deb` archive parsing, control-file metadata, and install logic |
| `rpm.rs` | `.rpm` archive parsing (lead/header/payload), metadata, and install logic |
| `repo.rs` | APT repository index fetching, caching, search, and dependency install |
| `rpmrepo.rs` | DNF/Fedora repository (`repomd.xml`/`primary.xml`) fetching and install |
| `elf.rs` | minimal ELF parser used to read `DT_NEEDED` entries and the interpreter |
| `resolve.rs` | resolves a binary's shared library dependencies against the store and system |
| `link.rs` | symlinks binaries and installs desktop entries/icons into `~/.local` |
| `version.rs` | Debian-style version comparison (`dpkg --compare-versions` semantics) |
| `src/bin/univ-store/` | the store TUI: `main.rs` (event loop), `app.rs` (state), `ui.rs` (rendering), `cmd.rs` (shells out to `univ`) |

## Building

```
cargo build --release      # builds both `univ` and `univ-store`
cargo install --path .     # installs both binaries
```

## Testing

```
cargo test
```
