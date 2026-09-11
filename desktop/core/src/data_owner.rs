//! Stable data-directory ownership survives a directory exchange during an offline upgrade.
use std::{fs::{self, File, OpenOptions}, os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt}, path::{Path, PathBuf}};
use anyhow::{anyhow, ensure, Context, Result};
use sha2::{Digest, Sha256};

pub struct Lease { directory: PathBuf, _stable: File, _legacy: Option<File> }

pub fn location(directory: &Path) -> Result<PathBuf> {
    let name = directory.file_name().ok_or_else(|| anyhow!("数据目录无效"))?;
    ensure!(name != "." && name != "..", "数据目录无效");
    let parent = directory.parent().ok_or_else(|| anyhow!("数据目录无效"))?.canonicalize()?;
    let path = parent.join(name);
    if let Ok(meta) = fs::symlink_metadata(&path) { ensure!(meta.file_type().is_dir(), "数据目录不能是链接或普通文件"); }
    Ok(path)
}

pub fn sidecar(directory: &Path, suffix: &str) -> Result<PathBuf> {
    let path = location(directory)?;
    let key = format!("{:x}", Sha256::digest(path.as_os_str().as_encoded_bytes()));
    Ok(path.parent().unwrap().join(format!(".vpnmgr-data-{key}.{suffix}")))
}

fn lock(path: &Path, create: bool) -> Result<File> {
    if let Ok(meta) = fs::symlink_metadata(path) { ensure!(meta.file_type().is_file(), "数据占用记录不是普通文件"); }
    let file = OpenOptions::new().read(true).write(true).create(create).truncate(false).mode(0o600).open(path)?;
    let meta = fs::symlink_metadata(path)?;
    let opened = file.metadata()?;
    ensure!(meta.file_type().is_file() && meta.dev() == opened.dev() && meta.ino() == opened.ino(), "数据占用记录在检查期间发生变化");
    file.try_lock().map_err(|_| anyhow!("这个数据目录正在使用。请先退出使用它的应用，再重试。"))?;
    Ok(file)
}

impl Lease {
    pub fn acquire(directory: &Path) -> Result<Self> {
        let parent = directory.parent().ok_or_else(|| anyhow!("数据目录无效"))?;
        fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
        let directory = location(directory)?;
        let stable = lock(&sidecar(&directory, "lock")?, true)?;
        // Coordinate with an already-running native build that predates the stable lock.
        let old = directory.join("native-owner.lock");
        let legacy = if old.try_exists()? { Some(lock(&old, false).context("旧版本仍占用这个数据目录")?) } else { None };
        Ok(Self { directory, _stable: stable, _legacy: legacy })
    }
    pub fn create_directory(&self) -> Result<()> {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(&self.directory)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ownership_survives_directory_rename_and_respects_legacy_owner() {
        let root = tempfile::tempdir().unwrap(); let data = root.path().join("data"); fs::create_dir(&data).unwrap();
        let lease = Lease::acquire(&data).unwrap();
        fs::rename(&data, root.path().join("previous")).unwrap(); fs::create_dir(&data).unwrap();
        assert!(Lease::acquire(&data).is_err());
        drop(lease);
        let old = lock(&data.join("native-owner.lock"), true).unwrap();
        assert!(Lease::acquire(&data).is_err()); drop(old);
        assert!(Lease::acquire(&data).is_ok());
    }
    #[test]
    fn linked_directories_and_lock_files_are_rejected() {
        let root = tempfile::tempdir().unwrap(); let data = root.path().join("data");
        fs::create_dir(&data).unwrap(); let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&data, &alias).unwrap(); assert!(Lease::acquire(&alias).is_err());
        let sentinel = root.path().join("sentinel"); fs::write(&sentinel, "keep").unwrap();
        std::os::unix::fs::symlink(&sentinel, sidecar(&data, "lock").unwrap()).unwrap();
        assert!(Lease::acquire(&data).is_err()); assert_eq!(fs::read_to_string(sentinel).unwrap(), "keep");
    }
}
