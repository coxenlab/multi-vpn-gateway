//! 启动相关的三个开关(方案 5.9):开机时自动打开、打开后自动连接、关闭窗口后继续运行,
//! 外加退出时在用的通道 `resume_channels`(由 [`crate::shutdown`] 记录,界面启动时按开关决定用不用)。
//! 存在 `<data_dir>/app-settings.json`(0600,原子写);外壳关窗时直接读这个文件的 `keep_running_on_close`。
//! 开机时自动打开 = 写入或删除 `~/Library/LaunchAgents/<应用标识>.launcher.plist`,不调 `launchctl`,
//! 下次登录生效。不用 `SMAppService`:ad-hoc 签名下用不了。只对通道管家身份生效:旧身份不写登录项,
//! 退出时也不写这个文件(旧应用的行为不变)。
use std::path::{Path, PathBuf};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use crate::api::{err500, err_detail};
use crate::AppState;

const FILE: &str = "app-settings.json";

/// PUT 与退出记录都是读-改-写,串行执行,互不覆盖。
static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct AppSettings {
    pub launch_at_login: bool,
    /// 两个应用并存期间默认关:自动连上同一账号会把旧应用的会话踢掉。
    pub auto_connect: bool,
    pub keep_running_on_close: bool,
    pub resume_channels: Vec<String>,
    /// 固定启动内存；VZ balloon 尚不能可靠归还宿主内存，不做在线压缩。
    pub vm_memory_gib: u8,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self { launch_at_login: false, auto_connect: false, keep_running_on_close: true, resume_channels: Vec::new(), vm_memory_gib: 2 }
    }
}

pub fn file_path(data_dir: &Path) -> PathBuf {
    data_dir.join(FILE)
}

/// 读回设置;文件不在或读不懂时用默认值。
pub fn load(path: &Path) -> AppSettings {
    let mut settings: AppSettings = std::fs::read(path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
    if !(2..=16).contains(&settings.vm_memory_gib) { settings.vm_memory_gib = 2; }
    settings
}

pub fn save(path: &Path, settings: &AppSettings) -> anyhow::Result<()> {
    crate::manager::atomic_write_0600(path, &serde_json::to_vec_pretty(settings)?)
}

/// 纯函数:把 PUT 的部分字段合并进当前设置。只认这四个字段,其余忽略;类型不对返回错误。
pub fn merge(current: &AppSettings, patch: &Value) -> Result<AppSettings, String> {
    let patch = patch.as_object().ok_or("请求体须为对象")?;
    let mut next = current.clone();
    for (key, slot) in [
        ("launch_at_login", &mut next.launch_at_login),
        ("auto_connect", &mut next.auto_connect),
        ("keep_running_on_close", &mut next.keep_running_on_close),
    ] {
        if let Some(value) = patch.get(key) {
            *slot = value.as_bool().ok_or(format!("{key} 须为布尔值"))?;
        }
    }
    if let Some(value) = patch.get("resume_channels") {
        next.resume_channels = value.as_array()
            .and_then(|ids| ids.iter().map(|id| id.as_str().map(str::to_string)).collect::<Option<Vec<_>>>())
            .ok_or("resume_channels 须为字符串数组")?;
    }
    if let Some(value) = patch.get("vm_memory_gib") {
        next.vm_memory_gib = value.as_u64().filter(|n| (2..=16).contains(n))
            .ok_or("运行环境内存须为 2–16 GiB 的整数")? as u8;
    }
    Ok(next)
}

/// 登录项的 launchd 标签。
pub fn launcher_label(bundle_id: &str) -> String {
    format!("{bundle_id}.launcher")
}

/// 登录项文件:`<HOME>/Library/LaunchAgents/<应用标识>.launcher.plist`。
pub fn launcher_path(home: &Path, bundle_id: &str) -> PathBuf {
    home.join("Library/LaunchAgents").join(format!("{}.plist", launcher_label(bundle_id)))
}

/// 纯函数:登录项 plist 文本。登录时 `open -b <应用标识>` 打开应用。
pub fn launcher_plist(bundle_id: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>/usr/bin/open</string>
        <string>-b</string>
        <string>{bundle_id}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
</dict>
</plist>
"#,
        label = launcher_label(bundle_id),
    )
}

/// 写入或删除登录项。`home` 由调用方传入(接口取 `$HOME`,单测用临时目录);删除时文件不在也算成功。
pub fn set_launch_at_login(home: &Path, bundle_id: &str, enable: bool) -> anyhow::Result<()> {
    anyhow::ensure!(!home.as_os_str().is_empty(), "HOME 未设置");
    let path = launcher_path(home, bundle_id);
    if enable {
        if let Some(dir) = path.parent() { std::fs::create_dir_all(dir)?; }
        crate::manager::atomic_write_0600(&path, launcher_plist(bundle_id).as_bytes())
    } else {
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }
}

/// 能不能改登录项:只有通道管家身份、允许宿主集成(正式档,非开发模式)时可以。旧应用没有这个开关,
/// 旧身份即使允许宿主集成也不写登录项。
pub fn login_item_allowed(id: &crate::identity::Identity, host_integrations_allowed: bool) -> bool {
    host_integrations_allowed && std::ptr::eq(id, &crate::identity::TONGDAO)
}

/// 覆盖 `resume_channels`,其余字段保持(退出清理调用)。
pub async fn set_resume_channels(data_dir: &Path, ids: Vec<String>) -> anyhow::Result<()> {
    let _guard = LOCK.lock().await;
    let path = file_path(data_dir);
    let mut settings = load(&path);
    settings.resume_channels = ids;
    save(&path, &settings)
}

/// PUT 的主体:合并、按需写入或删除登录项、保存。`allowed` = [`login_item_allowed`](开发模式、旧身份为假),
/// 不允许时改动 `launch_at_login` 返回 403;没改动它时不碰登录项。
pub async fn update(data_dir: &Path, allowed: bool, home: &Path, bundle_id: &str, patch: &Value)
    -> Result<AppSettings, (StatusCode, String)>
{
    let _guard = LOCK.lock().await;
    let path = file_path(data_dir);
    let current = load(&path);
    let next = merge(&current, patch).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let login_changed = next.launch_at_login != current.launch_at_login;
    if login_changed {
        if !allowed {
            return Err((StatusCode::FORBIDDEN, "开发版和隔离实例不能设置开机时自动打开".into()));
        }
        set_launch_at_login(home, bundle_id, next.launch_at_login)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("登录项写入失败: {e}")))?;
    }
    if let Err(e) = save(&path, &next) {
        if login_changed { let _ = set_launch_at_login(home, bundle_id, current.launch_at_login); }
        return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("设置保存失败: {e}")));
    }
    Ok(next)
}

/// `GET /api/app-settings`。
pub async fn get(State(st): State<AppState>) -> Json<AppSettings> {
    Json(load(&file_path(&st.cfg.data_dir)))
}

/// `PUT /api/app-settings`,可以只传部分字段;返回合并后的全部设置。
pub async fn put(State(st): State<AppState>, Json(b): Json<Value>) -> axum::response::Response {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let before = load(&file_path(&st.cfg.data_dir));
    let switches = |s: &AppSettings| json!({
        "launch_at_login": s.launch_at_login, "auto_connect": s.auto_connect,
        "keep_running_on_close": s.keep_running_on_close,
        "vm_memory_gib": s.vm_memory_gib,
    });
    let allowed = login_item_allowed(st.cfg.identity(), st.cfg.host_integrations_allowed());
    match update(&st.cfg.data_dir, allowed, &home, st.cfg.identity().bundle_id, &b).await {
        Ok(next) => {
            if switches(&next) != switches(&before) {
                crate::events::audit("app_settings", "启动设置已更改", json!({
                    "target_kind": "system", "target_name": "app_settings",
                    "before": switches(&before), "after": switches(&next), "result": "ok",
                }));
            }
            Json(next).into_response()
        }
        Err((status, msg)) if status == StatusCode::INTERNAL_SERVER_ERROR => {
            crate::events::audit_failed("app_settings", "启动设置更改失败", json!({
                "target_kind": "system", "target_name": "app_settings",
                "before": switches(&before), "result": "failed", "error": msg.as_str(),
            }));
            err500(&msg)
        }
        Err((status, msg)) => err_detail(status, &msg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "com.tongdao.desktop";

    #[test]
    fn defaults_and_unreadable_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = file_path(dir.path());
        let defaults = json!({"launch_at_login": false, "auto_connect": false, "keep_running_on_close": true, "resume_channels": [], "vm_memory_gib": 2});
        assert_eq!(serde_json::to_value(load(&path)).unwrap(), defaults);
        std::fs::write(&path, b"{").unwrap();
        assert_eq!(load(&path), AppSettings::default());
        // 缺的字段取默认值。
        std::fs::write(&path, br#"{"auto_connect": true}"#).unwrap();
        assert_eq!(load(&path), AppSettings { auto_connect: true, ..Default::default() });
    }

    #[test]
    fn merge_keeps_fields_not_sent() {
        let current = AppSettings { auto_connect: true, resume_channels: vec!["c1".into()], ..Default::default() };
        let next = merge(&current, &json!({"keep_running_on_close": false})).unwrap();
        assert_eq!(next, AppSettings { keep_running_on_close: false, ..current.clone() });
        let next = merge(&current, &json!({"resume_channels": [], "unknown": 1})).unwrap();
        assert_eq!(next, AppSettings { resume_channels: vec![], ..current.clone() });
        assert_eq!(merge(&current, &json!({})).unwrap(), current);
        assert!(merge(&current, &json!({"auto_connect": 1})).is_err());
        assert!(merge(&current, &json!({"keep_running_on_close": "false"})).is_err());
        assert!(merge(&current, &json!({"resume_channels": ["c1", 2]})).is_err());
        assert!(merge(&current, &json!([])).is_err());
    }

    #[test]
    fn memory_setting_validates_and_survives_other_updates() {
        let dir = tempfile::tempdir().unwrap(); let path = file_path(dir.path());
        let settings = merge(&AppSettings::default(), &json!({"vm_memory_gib": 3})).unwrap();
        save(&path, &settings).unwrap();
        assert_eq!(load(&path).vm_memory_gib, 3);
        assert_eq!(merge(&settings, &json!({"auto_connect": true})).unwrap().vm_memory_gib, 3);
        for value in [2, 16] {
            assert_eq!(merge(&settings, &json!({"vm_memory_gib":value})).unwrap().vm_memory_gib, value);
        }
        for value in [json!(1),json!(17),json!(3.5),json!("4"),json!(null)] {
            assert!(merge(&settings, &json!({"vm_memory_gib":value})).is_err());
        }
        std::fs::write(&path, br#"{"vm_memory_gib":1,"auto_connect":true}"#).unwrap();
        assert_eq!(load(&path).vm_memory_gib,2);
        assert!(load(&path).auto_connect);
    }

    #[test]
    fn launcher_plist_text() {
        assert_eq!(launcher_plist(ID), r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.tongdao.desktop.launcher</string>
    <key>ProgramArguments</key>
    <array>
        <string>/usr/bin/open</string>
        <string>-b</string>
        <string>com.tongdao.desktop</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
</dict>
</plist>
"#);
        assert_eq!(launcher_path(Path::new("/Users/x"), ID),
            PathBuf::from("/Users/x/Library/LaunchAgents/com.tongdao.desktop.launcher.plist"));
    }

    #[test]
    fn launcher_write_and_remove_in_temp_home() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let path = launcher_path(home.path(), ID);
        // 目录不存在时先建。
        set_launch_at_login(home.path(), ID, true).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), launcher_plist(ID));
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        set_launch_at_login(home.path(), ID, true).unwrap();
        set_launch_at_login(home.path(), ID, false).unwrap();
        assert!(!path.exists());
        // 已经没有时删除也算成功。
        set_launch_at_login(home.path(), ID, false).unwrap();
        assert!(set_launch_at_login(Path::new(""), ID, true).is_err());
    }

    #[tokio::test]
    async fn update_merges_and_follows_the_login_item() {
        let data = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let plist = launcher_path(home.path(), ID);
        let saved = || load(&file_path(data.path()));

        let next = update(data.path(), true, home.path(), ID, &json!({"auto_connect": true})).await.unwrap();
        assert_eq!(next, AppSettings { auto_connect: true, ..Default::default() });
        assert_eq!(saved(), next);
        assert!(!plist.exists());
        let meta = std::fs::metadata(file_path(data.path())).unwrap();
        assert_eq!(std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o777, 0o600);

        let next = update(data.path(), true, home.path(), ID, &json!({"launch_at_login": true})).await.unwrap();
        assert!(next.launch_at_login && next.auto_connect);
        assert_eq!(std::fs::read_to_string(&plist).unwrap(), launcher_plist(ID));

        let next = update(data.path(), true, home.path(), ID, &json!({"launch_at_login": false})).await.unwrap();
        assert!(!next.launch_at_login && next.auto_connect);
        assert!(!plist.exists());

        let err = update(data.path(), true, home.path(), ID, &json!({"auto_connect": "yes"})).await.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(saved(), next);
    }

    #[tokio::test]
    async fn update_without_host_integrations_forbids_only_login_changes() {
        let data = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let err = update(data.path(), false, home.path(), ID, &json!({"launch_at_login": true, "auto_connect": true})).await.unwrap_err();
        assert_eq!(err.0, StatusCode::FORBIDDEN);
        assert!(!file_path(data.path()).exists());
        assert!(!home.path().join("Library").exists());
        // 没改动 launch_at_login 时照常保存,也不碰登录项。
        let next = update(data.path(), false, home.path(), ID, &json!({"launch_at_login": false, "keep_running_on_close": false})).await.unwrap();
        assert_eq!(next, AppSettings { keep_running_on_close: false, ..Default::default() });
        assert!(!home.path().join("Library").exists());
    }

    #[tokio::test]
    async fn legacy_identity_forbids_login_item_and_writes_nothing() {
        use crate::identity::{TONGDAO, VPNMGR};
        assert!(login_item_allowed(&TONGDAO, true));
        assert!(!login_item_allowed(&TONGDAO, false));
        // 旧身份即使允许宿主集成(旧应用正式档)也不写登录项。
        assert!(!login_item_allowed(&VPNMGR, true));
        assert!(!login_item_allowed(&VPNMGR, false));
        let data = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        for patch in [json!({"launch_at_login": true}), json!({"launch_at_login": true, "auto_connect": true})] {
            let err = update(data.path(), login_item_allowed(&VPNMGR, true), home.path(), VPNMGR.bundle_id, &patch).await.unwrap_err();
            assert_eq!(err.0, StatusCode::FORBIDDEN);
        }
        // 临时 HOME 和数据目录下都没有写出任何文件。
        assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(data.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn resume_channels_replaced_other_fields_kept() {
        let data = tempfile::tempdir().unwrap();
        let path = file_path(data.path());
        save(&path, &AppSettings { auto_connect: true, resume_channels: vec!["old".into()], ..Default::default() }).unwrap();
        set_resume_channels(data.path(), vec!["c1".into(), "c2".into()]).await.unwrap();
        assert_eq!(load(&path), AppSettings { auto_connect: true, resume_channels: vec!["c1".into(), "c2".into()], ..Default::default() });
        set_resume_channels(data.path(), vec![]).await.unwrap();
        assert_eq!(load(&path).resume_channels, Vec::<String>::new());
    }
}
