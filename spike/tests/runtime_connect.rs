use spike::connect;

#[tokio::test]
#[ignore = "needs local colima running: colima start --vm-type vz"]
async fn bollard_pings_vm_docker_engine() {
    let docker = connect().await.expect("connect to VM docker.sock");
    let version = docker.version().await.expect("docker version");
    println!("VM Docker Engine version: {:?}", version.version);
    assert!(version.version.is_some());
}
