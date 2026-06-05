use spike::{connect, ensure_image, exec_capture, put_file, run_detached, rm_force};

#[tokio::test]
#[ignore = "needs local colima running"]
async fn put_file_lands_in_container() {
    let docker = connect().await.unwrap();
    ensure_image(&docker, "alpine:latest").await.unwrap();

    let name = "spike-put";
    rm_force(&docker, name).await.ok();
    run_detached(&docker, name, "alpine:latest", vec!["sleep", "60"]).await.unwrap();

    let payload = b"installer-bytes-v1";
    put_file(&docker, name, "/root", "client.bin", payload).await.unwrap();

    let back = exec_capture(&docker, name, vec!["cat", "/root/client.bin"]).await.unwrap();
    assert_eq!(back.trim_end(), "installer-bytes-v1");

    rm_force(&docker, name).await.ok();
}
