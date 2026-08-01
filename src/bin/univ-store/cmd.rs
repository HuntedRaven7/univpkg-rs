use std::io::{self, BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;

use serde::Deserialize;

#[derive(Clone, Deserialize, Debug, Default)]
pub struct Package {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub architecture: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub depends: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub repo: String,
}

pub fn univ_bin() -> String {
    if let Ok(b) = std::env::var("UNIV_BIN") {
        return b;
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
        && dir.join("univ").is_file()
    {
        return dir.join("univ").to_string_lossy().into_owned();
    }
    "univ".to_string()
}

fn run(args: &[&str]) -> io::Result<String> {
    let out = Command::new(univ_bin()).args(args).output()?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if !out.status.success() {
        let msg = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        return Err(io::Error::other(msg));
    }
    Ok(stdout)
}

pub fn list() -> io::Result<Vec<Package>> {
    let out = run(&["list", "--json"])?;
    serde_json::from_str(&out)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn search(query: &str) -> io::Result<Vec<Package>> {
    let out = Command::new(univ_bin())
        .args(["search", query, "--json"])
        .output()?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if let Ok(pkgs) = serde_json::from_str::<Vec<Package>>(&stdout) {
        return Ok(pkgs);
    }
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let msg = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    Err(io::Error::other(msg))
}

pub struct Task {
    pub title: String,
    pub rx: mpsc::Receiver<String>,
}

impl Task {
    pub fn spawn(title: String, args: Vec<String>) -> Task {
        let (tx, rx) = mpsc::channel();
        let bin = univ_bin();
        thread::spawn(move || {
            let mut child = match Command::new(&bin)
                .args(&args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
            {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(format!("error: {e}"));
                    return;
                }
            };
            let out = child.stdout.take().expect("stdout piped");
            let err = child.stderr.take().expect("stderr piped");
            let tx_out = tx.clone();
            let tx_err = tx.clone();
            let t1 = thread::spawn(move || pump(out, tx_out));
            let t2 = thread::spawn(move || pump(err, tx_err));
            let status = child.wait();
            let _ = t1.join();
            let _ = t2.join();
            let code = status
                .map(|s| s.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".to_string()))
                .unwrap_or_else(|e| e.to_string());
            let _ = tx.send(format!("[exit {code}]"));
        });
        Task { title, rx }
    }
}

fn pump(reader: impl Read, tx: mpsc::Sender<String>) {
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if tx.send(line.trim_end().to_string()).is_err() {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_list_json() {
        let j = r#"[{"name":"hello","version":"1.0","architecture":"amd64","description":"greets","depends":"libc6 (>= 2.34)"}]"#;
        let pkgs: Vec<Package> = serde_json::from_str(j).unwrap();
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].name, "hello");
        assert_eq!(pkgs[0].version, "1.0");
        assert_eq!(pkgs[0].depends, "libc6 (>= 2.34)");
        assert_eq!(pkgs[0].kind, "");
        assert_eq!(pkgs[0].repo, "");
    }

    #[test]
    fn parses_search_json() {
        let j = r#"[{"name":"git","version":"2.45","architecture":"x86_64","kind":"rpm","repo":"fedora","description":"scm"}]"#;
        let pkgs: Vec<Package> = serde_json::from_str(j).unwrap();
        assert_eq!(pkgs[0].kind, "rpm");
        assert_eq!(pkgs[0].repo, "fedora");
        assert_eq!(pkgs[0].depends, "");
    }
}
