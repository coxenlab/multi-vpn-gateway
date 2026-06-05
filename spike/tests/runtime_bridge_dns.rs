use spike::{connect, ensure_image, create_network_fresh, rm_force, rm_network,
            run_detached_on_net, run_capture_on_net};

#[tokio::test]
#[ignore = "needs local colima running"]
async fn container_resolves_peer_by_name_on_user_bridge() {
    let docker = connect().await.unwrap();
    ensure_image(&docker, "alpine:latest").await.unwrap();

    let net = "spike-net";
    let server = "vpn-test"; // 模拟 vpn-{id}
    rm_force(&docker, server).await.ok();
    rm_network(&docker, net).await.ok();
    create_network_fresh(&docker, net).await.unwrap();

    // server:在 1080 上回一行 "ok"(busybox nc 常驻)。
    run_detached_on_net(
        &docker, server, "alpine:latest", net,
        vec!["sh", "-c", "while true; do echo ok | nc -l -p 1080; done"],
    ).await.unwrap();

    // client:按名连 server:1080,成功则打印 RESOLVED。
    let out = run_capture_on_net(
        &docker, "spike-client", "alpine:latest", net,
        vec!["sh", "-c", "for i in $(seq 1 10); do nc -z -w2 vpn-test 1080 && { echo RESOLVED; break; }; sleep 1; done"],
    ).await.unwrap();

    assert!(out.contains("RESOLVED"), "peer not reachable by name: {out:?}");

    rm_force(&docker, server).await.ok();
    rm_network(&docker, net).await.ok();
}
