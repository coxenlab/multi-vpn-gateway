//! Offline configuration promotion. The VM must be stopped; no VM, container or host networking
//! mutation occurs here. A directory exchange retains the entire prior target for rollback.
use std::{fs, os::unix::fs::{DirBuilderExt, PermissionsExt}, path::{Path, PathBuf}};
use anyhow::{anyhow, ensure, Result};
use serde::{Deserialize, Serialize};
use crate::{config::Config, data_owner::{self, Lease}, upgrade};

const MARKER: &str = "upgrade-activation.json";

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Record {
    version: u32,
    id: String,
    phase: String,
    profile: String,
    source: PathBuf,
    candidate: PathBuf,
    target: PathBuf,
}

#[derive(Debug, Serialize)]
pub struct Status {
    pub phase: String,
    pub profile: String,
    pub source: PathBuf,
    pub target: PathBuf,
    pub retained_directory: PathBuf,
    pub runtime_verified: bool,
}

fn root(record: &Record) -> PathBuf { record.target.parent().unwrap().join(format!(".vpnmgr-upgrade-{}", record.id)) }
fn retained(record: &Record) -> PathBuf { root(record).join("data") }
fn journal(target: &Path) -> Result<PathBuf> { data_owner::sidecar(target, "upgrade.json") }

fn read_record(target: &Path) -> Result<Record> {
    let target = data_owner::location(target)?;
    let path = journal(&target)?;
    ensure!(fs::symlink_metadata(&path)?.file_type().is_file(), "升级切换记录不能是链接");
    let record: Record = serde_json::from_slice(&fs::read(path)?)?;
    ensure!(record.version == 1 && record.target == target && record.id.len() == 32
        && record.id.bytes().all(|byte| byte.is_ascii_hexdigit())
        && ["prepared", "active", "rolling_back", "rolled_back"].contains(&record.phase.as_str()), "升级切换记录不符合当前目录");
    Ok(record)
}

fn save_record(record: &Record, initial: bool) -> Result<()> {
    use std::io::Write;
    let target = journal(&record.target)?;
    let parent = target.parent().unwrap();
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.as_file().set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(&serde_json::to_vec_pretty(record)?)?; file.as_file().sync_all()?;
    if initial { file.persist_noclobber(&target).map_err(|error| error.error)?; }
    else { file.persist(&target).map_err(|error| error.error)?; }
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn has_marker(directory: &Path, id: &str) -> Result<bool> {
    ensure!(fs::symlink_metadata(directory)?.file_type().is_dir(), "切换数据目录不可用");
    let path = directory.join(MARKER);
    match fs::symlink_metadata(&path) {
        Ok(meta) => {
            ensure!(meta.file_type().is_file(), "升级目录标记不能是链接");
            let value: serde_json::Value = serde_json::from_slice(&fs::read(path)?)?;
            Ok(value["id"].as_str() == Some(id))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn inspect(record: &Record) -> Result<Status> {
    ensure!(fs::symlink_metadata(root(record))?.file_type().is_dir(), "升级保留目录不可用");
    let current = has_marker(&record.target, &record.id)?;
    let saved = has_marker(&retained(record), &record.id)?;
    ensure!(current != saved, "升级目录状态不明确，已停止自动处理；请保留两个目录并核对");
    let phase = if current {
        if record.phase == "rolling_back" { "rolling_back" }
        else if record.target.join(upgrade::PENDING).try_exists()? { "activating" } else { "active" }
    } else if matches!(record.phase.as_str(), "rolling_back" | "rolled_back") { "rolled_back" }
    else if record.phase == "prepared" { "prepared" }
    else { return Err(anyhow!("升级目录与切换记录不一致，请保留现场")); };
    Ok(Status { phase: phase.into(), profile: record.profile.clone(), source: record.source.clone(), target: record.target.clone(),
        retained_directory: retained(record), runtime_verified: false })
}

pub fn status(target: &Path) -> Result<Status> { inspect(&read_record(target)?) }
pub fn optional_status(target: &Path) -> Result<Option<Status>> {
    if !target.parent().is_some_and(Path::exists) || !journal(target)?.try_exists()? { return Ok(None); }
    status(target).map(Some)
}

pub fn validate_boot(target: &Path) -> Result<()> {
    if !target.parent().is_some_and(Path::exists) { return Ok(()); }
    if !journal(target)?.try_exists()? { return Ok(()); }
    let record = read_record(target)?;
    let current = has_marker(target, &record.id)?;
    let completed = match record.phase.as_str() {
        "active" => current,
        "rolled_back" | "rolling_back" => !current,
        "prepared" => current && !target.join(upgrade::PENDING).try_exists()?,
        _ => false,
    };
    ensure!(completed, "上次升级切换尚未完成，请先恢复切换或回退配置");
    Ok(())
}

/// Parent-directory lock held by the caller. This syscall exchanges both directory entries
/// atomically, so even SIGKILL cannot expose an absent or partly copied target directory.
#[cfg(target_os = "macos")]
fn exchange(a: &Path, b: &Path) -> Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    extern "C" { fn renamex_np(from: *const std::ffi::c_char, to: *const std::ffi::c_char, flags: u32) -> i32; }
    let a = CString::new(a.as_os_str().as_bytes())?; let b = CString::new(b.as_os_str().as_bytes())?;
    // RENAME_SWAP from the macOS SDK sys/stdio.h.
    let result = unsafe { renamex_np(a.as_ptr(), b.as_ptr(), 2) };
    if result != 0 { return Err(std::io::Error::last_os_error().into()); }
    Ok(())
}
#[cfg(not(target_os = "macos"))]
fn exchange(_: &Path, _: &Path) -> Result<()> { Err(anyhow!("配置切换目前仅支持 macOS")) }

fn sync_exchange(record: &Record) -> Result<()> {
    fs::File::open(root(record))?.sync_all()?;
    fs::File::open(record.target.parent().unwrap())?.sync_all()?;
    Ok(())
}

fn verify_profile(cfg: &Config, target: &Path, profile: &str) -> Result<()> {
    ensure!(data_owner::location(&cfg.data_dir)? == data_owner::location(target)?, "目标必须是当前应用配置的数据目录");
    ensure!(cfg.vm_profile == profile && !profile.is_empty() && profile != "default"
        && profile.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'), "运行环境标识不匹配，不能切换");
    ensure!(!cfg.dev_mode || profile != "vpnmgr", "开发模式不能切换日常运行环境");
    Ok(())
}

async fn require_stopped(cfg: &Config) -> Result<()> {
    ensure!(crate::vm::stopped_confirmed(&cfg.vm_profile).await?, "运行环境仍在使用。请先从旧版本断开并退出，再执行配置切换。");
    Ok(())
}

fn prepare_exchange(candidate: &Path, target: &Path, profile: &str) -> Result<Record> {
    let target = data_owner::location(target)?;
    ensure!(target.is_dir(), "请先初始化新版本的数据目录，再退出应用进行切换");
    ensure!(!journal(&target)?.try_exists()?, "这个目录已有升级切换记录，请先核对或恢复该记录");
    let candidate = candidate.canonicalize()?;
    let report = upgrade::validate_prepared(&candidate)?;
    ensure!(report.vm_profile == profile, "副本对应的运行环境与当前应用不一致");
    ensure!(!candidate.starts_with(&target) && !target.starts_with(&candidate), "升级副本不能包含目标目录或放在目标内");
    ensure!(report.source == target || !report.source.starts_with(&target) && !target.starts_with(&report.source), "原目录与目标目录不能互相嵌套");
    let record = Record { version: 1, id: format!("{:032x}", rand::random::<u128>()), phase: "prepared".into(),
        profile: profile.into(), source: report.source.clone(), candidate: candidate.clone(), target };
    fs::DirBuilder::new().mode(0o700).create(root(&record))?;
    let staged = retained(&record); fs::DirBuilder::new().mode(0o700).create(&staged)?;
    for name in &report.files {
        if name.contains('/') { fs::DirBuilder::new().mode(0o700).create(staged.join("logs"))?; }
        upgrade::write_private(&staged.join(name), &fs::read(candidate.join(name))?)?;
        if name.contains('/') { fs::File::open(staged.join("logs"))?.sync_all()?; }
    }
    upgrade::write_private(&staged.join("upgrade-review.json"), &serde_json::to_vec_pretty(&report)?)?;
    upgrade::write_private(&staged.join(upgrade::PENDING), b"Offline upgrade activation is pending.\n")?;
    upgrade::validate_prepared(&staged)?;
    upgrade::write_private(&staged.join(MARKER), &serde_json::to_vec(&serde_json::json!({"id": record.id}))?)?;
    fs::File::open(&staged)?.sync_all()?; fs::File::open(root(&record))?.sync_all()?;
    save_record(&record, true)?;
    Ok(record)
}

fn finish_activation(record: &mut Record) -> Result<Status> {
    let phase = inspect(record)?.phase;
    if phase == "prepared" {
        // Recheck the sealed original and staged data immediately before the exchange.
        upgrade::validate_prepared(&retained(record))?;
        exchange(&retained(record), &record.target)?; sync_exchange(record)?;
    } else { ensure!(matches!(phase.as_str(), "activating" | "active"), "该切换已回退，不能重复启用"); }
    if record.target.join(upgrade::PENDING).try_exists()? {
        // The original may equal the target and now lives in retained_directory; source
        // freshness was checked before exchange. Validate copied file hashes separately.
        upgrade::validate_copy(&record.target)?;
        fs::remove_file(record.target.join(upgrade::PENDING))?;
        fs::File::open(&record.target)?.sync_all()?;
    }
    record.phase = "active".into(); save_record(record, false)?;
    inspect(record)
}

pub async fn activate(candidate: &Path, cfg: &Config) -> Result<Status> {
    let report = upgrade::validate_prepared(candidate)?;
    let target = data_owner::location(&cfg.data_dir)?;
    verify_profile(cfg, &target, &report.vm_profile)?;
    let _target = Lease::acquire(&target)?;
    let _source = if report.source != target { Some(Lease::acquire(&report.source)?) } else { None };
    let _candidate = Lease::acquire(candidate)?;
    require_stopped(cfg).await?;
    let mut record = prepare_exchange(candidate, &target, &cfg.vm_profile)?;
    finish_activation(&mut record)
}

pub async fn resume(cfg: &Config) -> Result<Status> {
    let _target = Lease::acquire(&cfg.data_dir)?;
    let mut record = read_record(&cfg.data_dir)?;
    verify_profile(cfg, &record.target, &record.profile)?;
    let _retained = Lease::acquire(&retained(&record))?;
    let _source = if record.source != record.target { Some(Lease::acquire(&record.source)?) } else { None };
    require_stopped(cfg).await?;
    if record.phase == "rolling_back" { restore(&mut record) } else { finish_activation(&mut record) }
}

fn restore(record: &mut Record) -> Result<Status> {
    let phase = inspect(record)?.phase;
    if phase != "rolled_back" {
        record.phase = "rolling_back".into(); save_record(record, false)?;
        if phase != "prepared" { exchange(&record.target, &retained(record))?; sync_exchange(record)?; }
        record.phase = "rolled_back".into(); save_record(record, false)?;
    }
    inspect(record)
}

pub async fn rollback(cfg: &Config) -> Result<Status> {
    let _target = Lease::acquire(&cfg.data_dir)?;
    let mut record = read_record(&cfg.data_dir)?;
    verify_profile(cfg, &record.target, &record.profile)?;
    let _retained = Lease::acquire(&retained(&record))?;
    require_stopped(cfg).await?;
    restore(&mut record)
}

/// Finish bookkeeping only. Both data directories remain on disk; no backup is deleted.
pub fn finish(target: &Path) -> Result<Status> {
    let _target = Lease::acquire(target)?;
    let record = read_record(target)?;
    let status = inspect(&record)?;
    ensure!(matches!(status.phase.as_str(), "active" | "rolled_back"), "请先完成切换或回退，再结束升级记录");
    let history = root(&record).join("completed-upgrade.json");
    if history.try_exists()? {
        ensure!(fs::symlink_metadata(&history)?.file_type().is_file() && fs::read(&history)? == serde_json::to_vec_pretty(&record)?, "已有升级记录不匹配");
    } else { upgrade::write_private(&history, &serde_json::to_vec_pretty(&record)?)?; }
    fs::File::open(root(&record))?.sync_all()?;
    fs::remove_file(journal(target)?)?;
    fs::File::open(target.parent().unwrap())?.sync_all()?;
    Ok(status)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn fixture(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let (source, conn) = crate::upgrade::tests::source(root); drop(conn);
        let candidate = root.join("review"); upgrade::prepare(&source, &candidate).unwrap();
        let target = root.join("current"); fs::create_dir(&target).unwrap();
        fs::write(target.join("keep"), "previous target").unwrap();
        fs::create_dir(target.join("logs")).unwrap(); fs::write(target.join("logs/keep"), "previous log").unwrap();
        (source, candidate, target)
    }

    #[test]
    fn activation_and_rollback_preserve_original_and_entire_previous_target() {
        let temp = tempfile::tempdir().unwrap(); let (source, candidate, target) = fixture(temp.path());
        let original = fs::read(source.join("vpnmgr.db")).unwrap();
        let _lease = Lease::acquire(&target).unwrap();
        let mut record = prepare_exchange(&candidate, &target, "vpnmgr").unwrap();
        assert!(validate_boot(&target).is_err());
        assert_eq!(finish_activation(&mut record).unwrap().phase, "active");
        validate_boot(&target).unwrap(); assert!(!target.join(upgrade::PENDING).exists());
        assert_eq!(fs::read_to_string(retained(&record).join("logs/keep")).unwrap(), "previous log");
        let db = Connection::open(target.join("vpnmgr.db")).unwrap();
        let identity: (String, String) = db.query_row("SELECT mac,container_id FROM channels", [], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!(identity, ("02:01:02:03:04:05".into(), "owned-instance".into()));
        db.execute("UPDATE channels SET name='edited after activation'", []).unwrap(); drop(db);
        assert_eq!(restore(&mut record).unwrap().phase, "rolled_back"); validate_boot(&target).unwrap();
        assert_eq!(fs::read_to_string(target.join("keep")).unwrap(), "previous target");
        let saved = Connection::open(retained(&record).join("vpnmgr.db")).unwrap();
        assert_eq!(saved.query_row("SELECT name FROM channels", [], |r| r.get::<_, String>(0)).unwrap(), "edited after activation");
        assert_eq!(fs::read(source.join("vpnmgr.db")).unwrap(), original);
        assert_eq!(restore(&mut record).unwrap().phase, "rolled_back");
        drop(_lease); finish(&target).unwrap(); assert!(!journal(&target).unwrap().exists());
        assert!(retained(&record).join("vpnmgr.db").is_file());
    }

    #[test]
    fn same_directory_upgrade_and_stale_original_are_handled() {
        let temp = tempfile::tempdir().unwrap(); let (source, candidate, _) = fixture(temp.path());
        let mut record = prepare_exchange(&candidate, &source, "vpnmgr").unwrap();
        finish_activation(&mut record).unwrap();
        restore(&mut record).unwrap();
        validate_boot(&source).unwrap();
        assert!(source.join("master.key").is_file());
        finish(&source).unwrap();
        let stale = temp.path().join("stale"); upgrade::prepare(&source, &stale).unwrap();
        fs::write(source.join("routing_off"), "changed").unwrap();
        assert!(prepare_exchange(&stale, &source, "vpnmgr").is_err());
        assert!(!journal(&source).unwrap().exists());
    }

    #[test]
    fn pending_copy_is_rechecked_and_completed_data_does_not_depend_on_backup_retention() {
        let temp = tempfile::tempdir().unwrap(); let (_, candidate, target) = fixture(temp.path());
        let mut record = prepare_exchange(&candidate, &target, "vpnmgr").unwrap();
        exchange(&retained(&record), &target).unwrap(); sync_exchange(&record).unwrap();
        fs::write(target.join("routing_off"), "tampered after interruption").unwrap();
        assert!(finish_activation(&mut record).is_err()); assert!(target.join(upgrade::PENDING).exists());
        restore(&mut record).unwrap(); finish(&target).unwrap();
        let mut next = prepare_exchange(&candidate, &target, "vpnmgr").unwrap();
        finish_activation(&mut next).unwrap();
        fs::remove_dir_all(root(&next)).unwrap();
        validate_boot(&target).unwrap();
        assert!(status(&target).is_err()); // Rollback unavailable, but the active data is intact.
    }

    #[test]
    fn process_death_at_each_exchange_boundary_can_resume_or_roll_back() {
        for point in ["prepared", "exchanged", "unmarked", "rollback_before", "rollback_after"] {
            let temp = tempfile::tempdir().unwrap(); let (_, _, target) = fixture(temp.path());
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--ignored", "--exact", "upgrade_switch::tests::crash_child"])
                .env("VPNMGR_UPGRADE_TEST_ROOT", temp.path()).env("VPNMGR_UPGRADE_TEST_POINT", point)
                .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
            while !temp.path().join("at-boundary").exists() && std::time::Instant::now() < deadline {
                if child.try_wait().unwrap().is_some() { break; }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let reached = temp.path().join("at-boundary").exists();
            let _ = child.kill(); let _ = child.wait(); assert!(reached, "child did not reach {point}");
            let _lease = Lease::acquire(&target).unwrap();
            let mut record = read_record(&target).unwrap();
            if point.starts_with("rollback") { restore(&mut record).unwrap(); }
            else { finish_activation(&mut record).unwrap(); }
            validate_boot(&target).unwrap();
            if point.starts_with("rollback") { assert!(target.join("keep").exists()); }
            else {
                assert!(target.join("vpnmgr.db").exists());
                restore(&mut record).unwrap(); assert!(target.join("keep").exists());
            }
        }
    }

    #[test]
    #[ignore = "subprocess fixture invoked by process_death_at_each_exchange_boundary_can_resume_or_roll_back"]
    fn crash_child() {
        let directory = std::env::var_os("VPNMGR_UPGRADE_TEST_ROOT").unwrap(); let directory = Path::new(&directory);
        let point = std::env::var("VPNMGR_UPGRADE_TEST_POINT").unwrap();
        let target = directory.join("current"); let _lease = Lease::acquire(&target).unwrap();
        let mut record = prepare_exchange(&directory.join("review"), &target, "vpnmgr").unwrap();
        if point != "prepared" { exchange(&retained(&record), &target).unwrap(); sync_exchange(&record).unwrap(); }
        if matches!(point.as_str(), "unmarked" | "rollback_before" | "rollback_after") {
            fs::remove_file(target.join(upgrade::PENDING)).unwrap(); fs::File::open(&target).unwrap().sync_all().unwrap();
        }
        if point.starts_with("rollback") { record.phase = "rolling_back".into(); save_record(&record, false).unwrap(); }
        if point == "rollback_after" { exchange(&target, &retained(&record)).unwrap(); sync_exchange(&record).unwrap(); }
        fs::write(directory.join("at-boundary"), "ready").unwrap();
        loop { std::thread::park(); }
    }
}
