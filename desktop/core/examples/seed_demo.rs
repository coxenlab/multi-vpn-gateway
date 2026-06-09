//! Dev seeder:往本地 dev 库塞一条 demo 通道 + 两条规则,方便 `cargo run` 后总览页有卡片。
//! 用法:`cd desktop/core && cargo run --example seed_demo`(用默认 DATA_DIR,与 `cargo run` 一致)。
//! 幂等:重复跑会先删同 id 再插。这是开发便利脚本,非产品代码。
use vpnmgr_core::{config::Config, store};

fn main() -> anyhow::Result<()> {
    let cfg = Config::load();
    let db = cfg.db_path();
    store::init(&db)?;
    let key = store::master_key(&cfg.data_dir)?;

    let id = "demo0001";
    store::del_channel(&db, id)?; // 幂等

    let mut config = serde_json::Map::new();
    config.insert("server".into(), serde_json::json!("vpn.example.com"));
    config.insert("password".into(), serde_json::json!("demo-secret"));

    let ch = store::NewChannel {
        id: id.into(),
        name: "示例通道 (EasyConnect)".into(),
        vpn_type: "easyconnect".into(),
        server: "vpn.example.com".into(),
        ec_ver: "7.6.3".into(),
        login_method: "interactive".into(),
        username: "alice".into(),
        password: "demo-secret".into(),
        vnc_password: "vnc12345".into(),
        mac: "02:11:22:33:44:55".into(),
        probe_url: "https://intranet.example.com/".into(),
        status: "logged_in".into(),
    };
    store::add_channel(&db, &key, &ch, &config, &["password".to_string()])?;
    store::add_rule(&db, id, "domain", "example.com")?;
    store::add_rule(&db, id, "ip", "10.0.0.0/8")?;
    store::set_latency(&db, id, 38)?;

    println!("seeded demo channel '{id}' into {}", db.display());
    Ok(())
}
