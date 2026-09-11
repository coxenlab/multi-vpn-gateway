//! Translate known runtime output into product feedback without exposing paths, URLs or credentials.
//! An unfamiliar line is diagnostic output, not evidence that a stage finished.

fn percentage(line: &str) -> Option<u8> {
    line.split_whitespace().find_map(|word| {
        let value = word.strip_suffix('%')?.parse::<f64>().ok()?;
        (value.is_finite() && (0.0..=100.0).contains(&value)).then_some(value as u8)
    })
}

pub fn vm_detail(line: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    // Colima's download bar contains a percentage and a bar; avoid percentages in log URLs.
    if !lower.contains("http") && !lower.contains("msg=")
        && (line.contains('|') || line.contains('[')) {
        if let Some(percent) = percentage(line) { return Some(format!("准备运行环境文件 · 当前传输 {percent}%")); }
    }
    let detail = if lower.contains("decompressing") || lower.contains("extracting") {
        "正在解压运行环境文件…"
    } else if lower.contains("downloading") || lower.contains("download in progress") {
        "正在下载运行环境文件…"
    } else if lower.contains("using cached") {
        "正在读取已有运行环境文件…"
    } else if lower.contains("creating and starting") || lower.contains("starting vz") {
        "正在启动运行环境…"
    } else if lower.contains("waiting for the essential requirement") || lower.contains("waiting for ssh") {
        "正在等待运行环境响应…"
    } else if lower.contains("provisioning") {
        "正在配置运行环境…"
    } else { return None; };
    Some(detail.into())
}

pub fn image_detail(line: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    let detail = if lower.contains("extracting") {
        "正在解压分流组件"
    } else if lower.contains("downloading") || line.contains("拉取") && line.contains('从') {
        "正在下载分流组件"
    } else if line.starts_with("探测镜像源 ") {
        "正在检查可用下载源"
    } else if line.contains("不可达,跳过") || line.contains("失败:") {
        "当前下载源未成功，正在尝试其他下载源"
    } else if lower.contains("verifying checksum") {
        "正在校验分流组件"
    } else if lower.contains("pull complete") || lower.contains("download complete") || lower.contains("already exists") || line == "mihomo 镜像已存在" {
        "正在确认分流组件"
    } else { return None; };
    // Docker reports aggregate progress only for discovered layers, not total initialization.
    let percent = if lower.contains("downloading") || lower.contains("extracting") {
        line.rsplit(" · ").next().and_then(percentage)
    } else { None };
    Some(match percent {
        Some(percent) => format!("{detail} · 已知分层 {percent}%"),
        None => format!("{detail}…"),
    })
}

pub fn failure_hint(line: &str) -> Option<&'static str> {
    let lower = line.to_ascii_lowercase();
    if lower.contains("no space left on device") || lower.contains("disk quota exceeded") {
        Some("可用磁盘空间不足。请释放空间后重试连接。")
    } else if lower.contains("error validating sha sum") || lower.contains("checksum mismatch")
        || lower.contains("expected digest") && lower.contains("got") {
        Some("下载文件校验失败。请检查下载来源并重试；若仍失败，请查看运行诊断。")
    } else if lower.contains("resolve redirect failed") || lower.contains("failed to download")
        || lower.contains("network error") && lower.contains("download") {
        Some("运行环境文件下载未完成。请检查网络和下载源后重试。")
    } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_output_shows_stage_and_scoped_progress_without_log_material() {
        assert_eq!(vm_detail(r#"time=x msg="Downloading https://fixture.invalid/file?token=private""#).as_deref(), Some("正在下载运行环境文件…"));
        assert_eq!(vm_detail("    43% |████           | (43/100 MB, 1 MB/s)").as_deref(), Some("准备运行环境文件 · 当前传输 43%"));
        assert_eq!(vm_detail("2 MiB / 3 MiB [----] 66.67% 1MiB/s").as_deref(), Some("准备运行环境文件 · 当前传输 66%"));
        assert_eq!(vm_detail(r#"msg="Decompressing /private/user/file""#).as_deref(), Some("正在解压运行环境文件…"));
        assert_eq!(image_detail("private-mirror · Downloading · layer-private · 37%").as_deref(), Some("正在下载分流组件 · 已知分层 37%"));
        assert_eq!(image_detail("private-mirror · Extracting · layer-private · 100%").as_deref(), Some("正在解压分流组件 · 已知分层 100%"));
    }

    #[test]
    fn unknown_output_and_stage_completion_do_not_claim_runtime_success() {
        for line in ["done", "READY", "time=x msg=100%", "| NaN% |", "| 101% |", "https://fixture.invalid/43%"] { assert!(vm_detail(line).is_none()); }
        assert_eq!(image_detail("mirror · Pull complete · layer · 100%").as_deref(), Some("正在确认分流组件…"));
        assert_eq!(image_detail("mirror 失败: private-address").as_deref(), Some("当前下载源未成功，正在尝试其他下载源…"));
        assert!(failure_hint("slow after 120 seconds").is_none());
        assert!(failure_hint("context deadline exceeded").is_none());
        for text in ["no space left on device: private-path", "error validating SHA sum for private-url", "expected digest private, got private", "resolve redirect failed private-url"] {
            let hint = failure_hint(text).unwrap();
            assert!(!hint.contains("private"));
        }
    }
}
