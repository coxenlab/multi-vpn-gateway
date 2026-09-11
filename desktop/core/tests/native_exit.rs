//! Exercise the actual child-process exit boundary, with no real VM or host integration.
use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::test]
async fn native_child_drains_shutdown_events_before_exiting() {
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let colima = bin.join("colima");
    std::fs::write(&colima, "#!/bin/sh\n[ \"$*\" = 'stop vpnmgr-native-exit-test' ] || exit 73\nprintf 'stopping\\n' >&2 || exit 74\nprintf '%s\\n' \"$*\" >> \"$VPNMGR_EXIT_TEST_COMMANDS\"\n").unwrap();
    std::fs::set_permissions(&colima, std::fs::Permissions::from_mode(0o700)).unwrap();
    let commands = root.path().join("commands");
    for attempt in 0..6 {
        let data = root.path().join(format!("data-{attempt}"));
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_vpnmgr-core"))
            .env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("DATA_DIR", &data)
            .env("UI_PORT", "0")
            .env("VPNMGR_NATIVE_CHILD", "1")
            .env("VPNMGR_DEV_MODE", "1")
            .env("VPNMGR_MANAGED_VM", "1")
            .env("VPNMGR_VM_PROFILE", "vpnmgr-native-exit-test")
            .env("VPN_NET", "vpnmgr_native_exit_test")
            .env("VPNMGR_EXIT_TEST_COMMANDS", &commands)
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
            .kill_on_drop(true).spawn().unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(10), output.read_line(&mut line)).await.unwrap().unwrap();
        assert_eq!(serde_json::from_str::<serde_json::Value>(&line).unwrap()["event"], "ready");
        // A background app launcher may close its log pipe while the app is still alive.
        if attempt % 2 == 1 { drop(child.stderr.take()); }
        // Closing the owner pipe is the native app's normal clean-exit protocol.
        drop(child.stdin.take());
        let result = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output()).await.unwrap().unwrap();
        let error = String::from_utf8_lossy(&result.stderr);
        assert!(result.status.success(), "attempt {attempt}: {error}");
        assert!(!error.contains("panicked") && !error.contains("[events]"), "attempt {attempt}: {error}");
        let files = std::fs::read_dir(data.join("logs")).unwrap();
        let mut events = Vec::new();
        for file in files {
            let path = file.unwrap().path();
            if path.extension().is_some_and(|ext| ext == "jsonl") {
                for line in std::fs::read_to_string(path).unwrap().lines() {
                    let value: serde_json::Value = serde_json::from_str(line).unwrap();
                    events.push(value["event"].as_str().unwrap().to_string());
                }
            }
        }
        assert_eq!(events.iter().filter(|event| *event == "shutdown_begin").count(), 1, "attempt {attempt}: {events:?}");
        assert_eq!(events.iter().filter(|event| *event == "shutdown_done").count(), 1, "attempt {attempt}: {events:?}");
        assert_eq!(events.iter().filter(|event| *event == "vm_stopped").count(), 1, "attempt {attempt}: {events:?}");
    }
    assert_eq!(std::fs::read_to_string(commands).unwrap().lines().collect::<Vec<_>>(), vec!["stop vpnmgr-native-exit-test"; 6]);
}
