use spike::{connect, ensure_image, exec_capture, exec_inject_stdin, run_detached, rm_force};

#[tokio::test]
#[ignore = "needs local colima running"]
async fn injects_secret_via_stdin_not_argv() {
    let docker = connect().await.unwrap();
    ensure_image(&docker, "alpine:latest").await.unwrap();

    // 起一个常驻容器(sleep),后续对它 exec。
    let name = "spike-exec";
    rm_force(&docker, name).await.ok();
    run_detached(&docker, name, "alpine:latest", vec!["sleep", "60"]).await.unwrap();

    // 关键:把 secret 经 stdin 喂给 `cat > /tmp/injected`,EOF 收尾。
    let secret = b"hunter2-from-stdin";
    exec_inject_stdin(&docker, name, vec!["sh", "-c", "cat > /tmp/injected"], secret)
        .await
        .unwrap();

    // 读回文件,断言 == secret(证明 stdin 通路字节级正确)。
    let back = exec_capture(&docker, name, vec!["cat", "/tmp/injected"]).await.unwrap();
    assert_eq!(back.trim_end(), "hunter2-from-stdin");

    // 断言 secret 不在该容器任何进程的 argv 里。
    let ps = exec_capture(&docker, name, vec!["sh", "-c", "cat /proc/*/cmdline | tr '\\0' ' '"]).await.unwrap();
    assert!(!ps.contains("hunter2-from-stdin"), "secret leaked into argv: {ps}");

    rm_force(&docker, name).await.ok();
}
