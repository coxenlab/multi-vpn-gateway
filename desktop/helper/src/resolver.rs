//! Scoped resolver ownership. Callers supply root-owned directories; tests use scratch.
//! No host paths or shell commands live here. A journaled nonce distinguishes our
//! files from pre-existing files, even across a crash between create and bookkeeping.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

const MARK: &str = "# tongdao-helper managed";
const PREFIX: &str = "tongdao.";

pub fn valid_domain(domain: &str) -> bool {
    !domain.is_empty() && domain.len() <= 253
        && domain != "local" && !domain.ends_with(".local")
        && domain.split('.').all(|label| !label.is_empty() && label.len() <= 63
            && label.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()
                || b == b'-' || b == b'_'))
}

#[derive(Default, Serialize, Deserialize)]
struct Journal {
    files: BTreeMap<String, String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub owned: Vec<String>,
    pub taken: Vec<String>,
}

pub struct Resolvers {
    root: PathBuf,
    journal: PathBuf,
    state: Journal,
}

fn invalid(message: &str) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, message) }

fn read_regular(path: &Path) -> io::Result<String> {
    let mut file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path)?;
    if !file.metadata()?.is_file() { return Err(invalid("resolver ownership path is not a file")); }
    let mut text = String::new();
    (&mut file).take(1024 * 1024 + 1).read_to_string(&mut text)?;
    if text.len() > 1024 * 1024 { return Err(invalid("resolver ownership file too large")); }
    Ok(text)
}

fn nonce() -> io::Result<String> {
    let mut bytes = [0; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

impl Resolvers {
    pub fn open(root: &Path, journal: &Path) -> io::Result<Self> {
        // Do not follow a substituted directory into another resolver namespace.
        for dir in [root, journal.parent().ok_or_else(|| invalid("missing journal parent"))?] {
            if !fs::symlink_metadata(dir)?.file_type().is_dir() {
                return Err(invalid("resolver directory must be a real directory"));
            }
        }
        let state: Journal = match read_regular(journal) {
            Ok(text) => serde_json::from_str(&text).map_err(io::Error::other)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Journal::default(),
            Err(e) => return Err(e),
        };
        for (domain, contents) in &state.files {
            if !valid_domain(domain) || !contents.starts_with(&format!("{MARK}\n# owner ")) {
                return Err(invalid("invalid resolver ownership journal"));
            }
        }
        Ok(Self { root: root.into(), journal: journal.into(), state })
    }

    fn path(&self, domain: &str) -> PathBuf { self.root.join(format!("{PREFIX}{domain}")) }

    fn save(&self) -> io::Result<()> {
        let pending = self.journal.with_extension(format!("{}.pending", nonce()?));
        let result = (|| {
            let mut f = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&pending)?;
            serde_json::to_writer(&mut f, &self.state).map_err(io::Error::other)?;
            f.sync_all()?;
            fs::rename(&pending, &self.journal)?;
            std::fs::File::open(self.journal.parent().unwrap())?.sync_all()
        })();
        if result.is_err() { let _ = fs::remove_file(&pending); }
        result
    }

    fn publish(&self, path: &Path, contents: &str) -> io::Result<()> {
        // Stage outside /etc/resolver. Publishing a complete file with link(2)
        // is atomic and refuses to replace any existing file or symlink.
        // If the roots are on different filesystems, fail without touching DNS.
        let pending = self.journal.with_extension(format!("{}.resolver", nonce()?));
        let result = (|| {
            let mut f = OpenOptions::new().write(true).create_new(true).mode(0o644).open(&pending)?;
            f.write_all(contents.as_bytes())?;
            f.sync_all()?;
            fs::hard_link(&pending, path)?;
            std::fs::File::open(&self.root)?.sync_all()
        })();
        let _ = fs::remove_file(&pending);
        result
    }

    fn remove_owned(&mut self, domain: &str) -> io::Result<()> {
        let Some(expected) = self.state.files.get(domain) else { return Ok(()) };
        let path = self.path(domain);
        match read_regular(&path) {
            Ok(actual) if &actual == expected => fs::remove_file(path)?,
            Ok(_) => {}, // User replaced/edited it: relinquish, never delete.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {},
            Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {}, // Foreign symlink.
            Err(e) => return Err(e),
        }
        self.state.files.remove(domain);
        self.save()
    }

    /// Empty desired set cleans up. A failed write returns an error, never readiness.
    /// Invoke only after the engine and required routes are confirmed ready.
    pub fn reconcile(&mut self, domains: &[String], port: u16) -> io::Result<Status> {
        if domains.iter().any(|d| !valid_domain(d)) || (!domains.is_empty() && port < 1024) {
            return Err(invalid("invalid resolver domain or port"));
        }
        let wanted: BTreeSet<_> = domains.iter().cloned().collect();
        let stale: Vec<_> = self.state.files.keys().filter(|d| !wanted.contains(*d)).cloned().collect();
        for domain in stale { self.remove_owned(&domain)?; }
        let mut status = Status::default();
        for domain in wanted {
            let tail = format!("domain {domain}\nnameserver 127.0.0.1\nport {port}\n");
            if let Some(expected) = self.state.files.get(&domain) {
                let matches = read_regular(&self.path(&domain)).is_ok_and(|actual| &actual == expected);
                if matches && expected.ends_with(&tail) {
                    status.owned.push(domain);
                    continue;
                }
                self.remove_owned(&domain)?;
            }
            // Journal intent first: after any crash, only this unique content is ours.
            let contents = format!("{MARK}\n# owner {}\n{tail}", nonce()?);
            self.state.files.insert(domain.clone(), contents.clone());
            self.save()?;
            let path = self.path(&domain);
            match self.publish(&path, &contents) {
                Ok(()) => { status.owned.push(domain); }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    self.state.files.remove(&domain);
                    self.save()?;
                    status.taken.push(domain);
                }
                Err(e) => return Err(e),
            }
        }
        Ok(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("tongdao-resolver-{}", nonce().unwrap()));
            fs::create_dir(&p).unwrap(); Self(p)
        }
        fn open(&self) -> Resolvers { Resolvers::open(&self.0, &self.0.join("owned.json")).unwrap() }
    }
    impl Drop for Scratch { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }
    fn domains() -> Vec<String> { vec!["corp.test".into()] }

    #[test]
    fn rejects_invalid_input_before_changing_any_files() {
        let dir = Scratch::new(); let mut r = dir.open();
        for bad in ["", "../corp", ".corp", "corp.", "a..b", "LOCAL", "local", "a.local", "a/b", "a\0b", "*.corp", "域名.test"] {
            assert!(r.reconcile(&[bad.into()], 12000).is_err(), "{bad:?}");
        }
        assert!(r.reconcile(&domains(), 53).is_err());
        assert!(!valid_domain(&format!("{}.test", "a".repeat(64))));
        assert!(!valid_domain(&vec!["a".repeat(63); 4].join(".")));
        assert!(valid_domain("xn--fsqu00a.corp-test"));
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 0);
    }

    #[test]
    fn reconciles_port_changes_missing_files_restart_and_stop() {
        let dir = Scratch::new(); let mut r = dir.open();
        assert_eq!(r.reconcile(&domains(), 12000).unwrap().owned, domains());
        let path = r.path("corp.test"); let first = fs::read_to_string(&path).unwrap();
        r.reconcile(&domains(), 12000).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), first);
        r.reconcile(&domains(), 12001).unwrap();
        assert!(fs::read_to_string(&path).unwrap().contains("port 12001\n"));
        fs::remove_file(&path).unwrap();
        assert_eq!(r.reconcile(&domains(), 12001).unwrap().owned, domains());
        drop(r);
        dir.open().reconcile(&[], 0).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn preserves_foreign_files_edited_files_and_symlinks() {
        let dir = Scratch::new(); let mut r = dir.open(); let path = r.path("corp.test");
        fs::write(&path, format!("{MARK}\nforeign\n")).unwrap();
        assert_eq!(r.reconcile(&domains(), 12000).unwrap().taken, domains());
        assert!(fs::read_to_string(&path).unwrap().contains("foreign"));
        fs::remove_file(&path).unwrap(); r.reconcile(&domains(), 12000).unwrap();
        fs::write(&path, "user edited").unwrap();
        r.reconcile(&[], 0).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "user edited");
        fs::remove_file(&path).unwrap(); r.reconcile(&domains(), 12000).unwrap();
        fs::remove_file(&path).unwrap();
        let target = dir.0.join("foreign"); fs::write(&target, "do not touch").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert_eq!(r.reconcile(&domains(), 12000).unwrap().taken, domains());
        assert_eq!(fs::read_to_string(&target).unwrap(), "do not touch");
        assert!(fs::symlink_metadata(&path).unwrap().file_type().is_symlink());
    }

    #[test]
    fn rejects_corrupt_journal_and_directory_symlink() {
        let dir = Scratch::new();
        fs::write(dir.0.join("owned.json"), r##"{"files":{"../escape":"# tongdao-helper managed\n# owner x"}}"##).unwrap();
        assert!(Resolvers::open(&dir.0, &dir.0.join("owned.json")).is_err());
        let link = dir.0.join("link"); std::os::unix::fs::symlink(&dir.0, &link).unwrap();
        assert!(Resolvers::open(&link, &dir.0.join("other.json")).is_err());
    }

    #[test]
    fn recovers_journaled_intent_with_or_without_published_file() {
        for published in [false, true] {
            let dir = Scratch::new(); let mut r = dir.open();
            let contents = format!("{MARK}\n# owner {}\ndomain corp.test\nnameserver 127.0.0.1\nport 12000\n", nonce().unwrap());
            r.state.files.insert("corp.test".into(), contents.clone()); r.save().unwrap();
            let path = r.path("corp.test");
            if published { r.publish(&path, &contents).unwrap(); }
            drop(r);
            dir.open().reconcile(&[], 0).unwrap();
            assert!(!path.exists());
            assert!(dir.open().state.files.is_empty());
        }
    }
}
