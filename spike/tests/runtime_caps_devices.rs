use spike::{connect, ensure_image, run_capture_privileged};

#[tokio::test]
#[ignore = "needs local colima running"]
async fn device_tun_cap_and_sysctl_apply() {
    let docker = connect().await.unwrap();
    ensure_image(&docker, "alpine:latest").await.unwrap();

    // 容器内:
    //   1. ls /dev/net/tun — 节点存在(保留原有检查)
    //   2. ip tuntap add dev tun0 mode tun — 可用性证明(open()+TUNSETIFF,需 NET_ADMIN + cgroup 放行)
    //   3. ip link show tun0 — 确认接口创建成功
    //   4. IP_FORWARD= / ROUTE_LOCALNET= — 标记行便于逐项断言
    //
    // preferred variant: iproute2 的 ip tuntap(完整 open+TUNSETIFF 证明);
    // busybox ip 无 tuntap 子命令,需先 apk add iproute2。
    let out = run_capture_privileged(
        &docker, "spike-caps", "alpine:latest",
        vec!["sh", "-c",
            "ls /dev/net/tun \
             && apk add --no-cache iproute2 >/dev/null 2>&1 \
             && ip tuntap add dev tun0 mode tun \
             && ip link show tun0 \
             && echo IP_FORWARD=$(cat /proc/sys/net/ipv4/ip_forward) \
             && echo ROUTE_LOCALNET=$(cat /proc/sys/net/ipv4/conf/default/route_localnet)",
        ],
    ).await.unwrap();

    // 原有检查:节点存在
    assert!(out.contains("/dev/net/tun"), "tun device node missing: {out:?}");

    // 新增 usability 检查:接口创建成功 = open()+TUNSETIFF ioctl 均通过
    assert!(out.contains("tun0"), "tun usability probe failed (ip tuntap add dev tun0): {out:?}");

    // Fix 3:oss sysctl ip_forward=1
    assert!(
        out.lines().any(|l| l.trim() == "IP_FORWARD=1"),
        "ip_forward sysctl not applied: {out:?}",
    );

    // Fix 3:aTrust sysctl route_localnet=1
    assert!(
        out.lines().any(|l| l.trim() == "ROUTE_LOCALNET=1"),
        "route_localnet sysctl not applied: {out:?}",
    );
}
