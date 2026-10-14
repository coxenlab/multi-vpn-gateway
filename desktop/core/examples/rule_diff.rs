//! G-03 差异清单:对一份配置导出,列出内层入口规则在改动前后的差别(域名顺序、出错通道被阻止)。
//! 用法:`cargo run --example rule_diff -- <导出文件> [出错的通道名...]`。
//! 只读这一个文件并打印,不用 `Config::load`,不碰数据库和网络。导出文件里有凭据,由用户自己运行、自己看。
use vpnmgr_core::manager;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args.next().ok_or_else(|| anyhow::anyhow!("用法: rule_diff <导出文件> [出错的通道名...]"))?;
    let error_names: Vec<String> = args.collect();
    let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
    anyhow::ensure!(doc["kind"] == "vpnmgr-export" && doc["channels"].is_array(), "不是配置导出文件");

    let lines = manager::rule_diff(&doc, &error_names);
    println!("通道 {} 条,当作出错 {} 条", doc["channels"].as_array().map_or(0, Vec::len), error_names.len());
    if lines.is_empty() {
        println!("没有差异");
    }
    for line in lines {
        println!("{line}");
    }
    Ok(())
}
