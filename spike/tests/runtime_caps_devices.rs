use spike::{connect, ensure_image, run_capture_privileged};

#[tokio::test]
#[ignore = "needs local colima running"]
async fn device_tun_cap_and_sysctl_apply() {
    let docker = connect().await.unwrap();
    ensure_image(&docker, "alpine:latest").await.unwrap();

    // 容器内:确认 /dev/net/tun 存在 + ip_forward sysctl 已被设为 1。
    let out = run_capture_privileged(
        &docker, "spike-caps", "alpine:latest",
        vec!["sh", "-c", "ls /dev/net/tun && cat /proc/sys/net/ipv4/ip_forward"],
    ).await.unwrap();

    assert!(out.contains("/dev/net/tun"), "tun device missing: {out:?}");
    assert!(out.lines().any(|l| l.trim() == "1"), "ip_forward sysctl not applied: {out:?}");
}
