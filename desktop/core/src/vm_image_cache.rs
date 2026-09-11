//! Installed native resources are seeded only on an explicit runtime connection, never UI boot.
use anyhow::{Context, Result};
use std::{io::{Read, Write}, path::{Path, PathBuf}};

pub async fn seed_bundled() -> Result<usize> {
    let Some(source) = std::env::var_os("VPNMGR_BUNDLED_VM_IMAGE_DIR").filter(|value| !value.is_empty()) else { return Ok(0) };
    let home = std::env::var_os("HOME").filter(|value| !value.is_empty()).context("无法定位 VM 缓存目录")?;
    let cache = PathBuf::from(home).join("Library/Caches/colima/caches");
    tokio::task::spawn_blocking(move || seed(Path::new(&source), &cache)).await?
}

fn cache_key(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn seed(source: &Path, cache: &Path) -> Result<usize> {
    let mut images = Vec::new();
    for entry in std::fs::read_dir(source).context("读取内置 VM 镜像失败")? {
        let entry = entry?;
        let name = entry.file_name();
        if !name.to_str().is_some_and(cache_key) { continue; }
        anyhow::ensure!(entry.file_type()?.is_file() && entry.metadata()?.len() > 0, "内置 VM 镜像不是有效文件");
        images.push((name, entry.path()));
    }
    anyhow::ensure!(!images.is_empty(), "未找到内置 VM 镜像");
    std::fs::create_dir_all(cache).context("创建 VM 缓存目录失败")?;
    let mut copied = 0;
    for (name, path) in images {
        let destination = cache.join(name);
        if destination.try_exists()? { continue; }
        let mut input = std::fs::File::open(path).context("打开内置 VM 镜像失败")?;
        if publish(&mut input, &destination)? { copied += 1; }
    }
    Ok(copied)
}

fn publish(input: &mut impl Read, destination: &Path) -> Result<bool> {
    let mut temporary = tempfile::NamedTempFile::new_in(destination.parent().context("VM 缓存位置无效")?)?;
    std::io::copy(input, &mut temporary).context("复制内置 VM 镜像失败")?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    // A failed/interrupted copy never leaves a truncated target; another app may have
    // populated the shared Colima cache concurrently, so never replace an existing file.
    match temporary.persist_noclobber(destination) {
        Ok(_) => Ok(true),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error.error).context("保存 VM 镜像缓存失败"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const KEY: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn seeds_once_and_preserves_existing_cache() -> Result<()> {
        let source = tempfile::tempdir()?;
        let root = tempfile::tempdir()?;
        let cache = root.path().join("cache");
        std::fs::write(source.path().join(KEY), b"bundled vm image")?;
        std::fs::write(source.path().join("README.txt"), b"ignored")?;
        assert_eq!(seed(source.path(), &cache)?, 1);
        assert_eq!(std::fs::read(cache.join(KEY))?, b"bundled vm image");
        std::fs::write(source.path().join(KEY), b"new bundle must not replace existing cache")?;
        assert_eq!(seed(source.path(), &cache)?, 0);
        assert_eq!(std::fs::read(cache.join(KEY))?, b"bundled vm image");
        assert_eq!(std::fs::read_dir(&cache)?.count(), 1);
        Ok(())
    }

    #[test]
    fn rejects_missing_empty_or_symlinked_images_without_creating_cache() -> Result<()> {
        let source = tempfile::tempdir()?;
        let root = tempfile::tempdir()?;
        let cache = root.path().join("cache");
        assert!(seed(source.path(), &cache).is_err());
        std::fs::write(source.path().join(KEY), b"")?;
        assert!(seed(source.path(), &cache).is_err());
        std::fs::remove_file(source.path().join(KEY))?;
        let external = root.path().join("outside");
        std::fs::write(&external, b"outside")?;
        std::os::unix::fs::symlink(&external, source.path().join(KEY))?;
        assert!(seed(source.path(), &cache).is_err());
        assert!(!cache.exists());
        Ok(())
    }

    #[test]
    fn failed_copy_cleans_partial_and_concurrent_writer_is_preserved() -> Result<()> {
        struct Broken(bool);
        impl Read for Broken {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                if self.0 { return Err(std::io::Error::other("injected read failure")); }
                self.0 = true; buffer[0] = 1; Ok(1)
            }
        }
        let cache = tempfile::tempdir()?;
        let target = cache.path().join(KEY);
        assert!(publish(&mut Broken(false), &target).is_err());
        assert_eq!(std::fs::read_dir(cache.path())?.count(), 0);
        std::fs::write(&target, b"other writer")?;
        assert!(!publish(&mut &b"bundle"[..], &target)?);
        assert_eq!(std::fs::read(&target)?, b"other writer");
        assert_eq!(std::fs::read_dir(cache.path())?.count(), 1);
        Ok(())
    }
}
