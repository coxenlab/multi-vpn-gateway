//! 从实际运行文件读取 Go 构建信息；无需在用户机器安装 Go / Xcode。
//! 仅支持当前分发的薄 Mach-O 64 位文件，其他格式返回未知，不按 Lima 名称猜依赖版本。
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{anyhow, ensure, Result};
use serde::Serialize;

#[derive(Debug, Clone, Default, Serialize)]
pub struct RuntimeInfo {
    pub go: String,
    pub lima: Option<String>,
    pub gvisor_tap_vsock: Option<String>,
    pub gvisor_replaced: bool,
    pub gvisor_patch: Option<String>,
    /// 已核对上游源码的默认上限，仅是诊断参照，不等于当前 SYN_SENT 数量。
    pub default_dial_limit: Option<usize>,
}

fn u32le(b: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(b.get(offset..offset + 4).ok_or_else(|| anyhow!("short Mach-O"))?.try_into()?))
}

fn var_string<'a>(data: &mut &'a [u8]) -> Result<&'a [u8]> {
    let mut length = 0_u64;
    for shift in (0..70).step_by(7) {
        let (&byte, rest) = data.split_first().ok_or_else(|| anyhow!("short Go build info"))?;
        *data = rest;
        ensure!(shift != 63 || byte < 2, "invalid build info length");
        length |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            let size = usize::try_from(length)?;
            ensure!(size <= data.len(), "short build info string");
            let (value, rest) = data.split_at(size);
            *data = rest;
            return Ok(value);
        }
    }
    Err(anyhow!("invalid build info length"))
}

fn decode(data: &[u8]) -> Result<RuntimeInfo> {
    // Go 1.18+ inline format: https://go.dev/src/debug/buildinfo/buildinfo.go
    ensure!(data.starts_with(b"\xff Go buildinf:") && data.len() >= 32 && data[15] & 2 != 0,
        "unsupported Go build info");
    let mut tail = &data[32..];
    let go = std::str::from_utf8(var_string(&mut tail)?)?.to_owned();
    let framed = var_string(&mut tail)?;
    ensure!(framed.len() >= 33 && framed[framed.len() - 17] == b'\n', "invalid module framing");
    let modules = std::str::from_utf8(&framed[16..framed.len() - 16])?;
    let mut info = RuntimeInfo { go, ..Default::default() };
    let mut dependency_checksum = false;
    let mut packaged_lima = false;
    let mut backport = false;
    let mut lines = modules.lines().peekable();
    while let Some(line) = lines.next() {
        let fields: Vec<_> = line.split('\t').collect();
        if fields.first() == Some(&"dep") && fields.get(1) == Some(&"github.com/containers/gvisor-tap-vsock") {
            info.gvisor_tap_vsock = fields.get(2).map(|s| s.to_string());
            info.gvisor_replaced = lines.peek().is_some_and(|s| s.starts_with("=>\t"));
            dependency_checksum = fields.get(3).is_some_and(|s| !s.is_empty());
        }
        // -trimpath 会隐藏 ldflags；构建标签仍可从实际二进制读回，不依赖旁置 manifest。
        if let Some(tags) = line.strip_prefix("build\t-tags=") {
            let tags: Vec<_> = tags.trim_matches('"').split(',').collect();
            packaged_lima = tags.contains(&"vpnmgr_lima_2_1_2");
            backport = tags.contains(&"vpnmgr_gvisor_698_8b4db4a");
        }
        if let Some((_, version)) = line.split_once("github.com/lima-vm/lima/v2/pkg/version.Version=") {
            info.lima = version.split([' ', '"', '\'']).next().map(str::to_owned);
        }
    }
    if packaged_lima {
        info.lima = Some(if backport { "v2.1.2-vpnmgr.698.8b4db4a" } else { "v2.1.2" }.into());
    }
    // v0.8.9 源码的 TCP forwarder 默认值；自定义 replacement / 未知版本不套此值。
    if info.gvisor_tap_vsock.as_deref() == Some("v0.8.9") && !info.gvisor_replaced {
        if packaged_lima && backport {
            info.gvisor_patch = Some("698@8b4db4a (v0.8.9 backport)".into());
            info.default_dial_limit = Some(128);
        } else if dependency_checksum || packaged_lima {
            info.default_dial_limit = Some(10);
        }
    }
    Ok(info)
}

/// 只读 Mach-O load commands 与 __go_buildinfo section，避免把整个 runtime 读入内存。
pub fn read(path: &Path) -> Result<RuntimeInfo> {
    let mut file = std::fs::File::open(path)?;
    let mut header = [0; 32];
    file.read_exact(&mut header)?;
    ensure!(u32le(&header, 0)? == 0xfeedfacf, "unsupported executable format");
    let command_count = u32le(&header, 16)?;
    let command_bytes = u32le(&header, 20)? as usize;
    ensure!(command_count <= 4096 && command_bytes <= 4 * 1024 * 1024, "oversized Mach-O commands");
    let mut commands = vec![0; command_bytes];
    file.read_exact(&mut commands)?;
    let mut remaining = commands.as_slice();
    for _ in 0..command_count {
        let kind = u32le(remaining, 0)?;
        let size = u32le(remaining, 4)? as usize;
        ensure!(size >= 8 && size <= remaining.len(), "invalid Mach-O command");
        let (command, rest) = remaining.split_at(size);
        remaining = rest;
        if kind != 0x19 { continue; } // LC_SEGMENT_64
        ensure!(size >= 72, "short Mach-O segment");
        let sections = u32le(command, 64)? as usize;
        ensure!(sections <= (size.saturating_sub(72)) / 80, "invalid Mach-O sections");
        for section in command[72..].chunks_exact(80).take(sections) {
            if section[..16].split(|b| *b == 0).next() != Some(b"__go_buildinfo".as_slice()) { continue; }
            let length = u64::from_le_bytes(section[40..48].try_into()?);
            ensure!(length <= 256 * 1024, "oversized build info");
            let offset = u32le(section, 48)?;
            file.seek(SeekFrom::Start(u64::from(offset)))?;
            let mut data = vec![0; length as usize];
            file.read_exact(&mut data)?;
            return decode(&data);
        }
    }
    Err(anyhow!("Go build info section missing"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_blob(modules: &str) -> Vec<u8> {
        fn append(data: &mut Vec<u8>, value: &[u8]) {
            let mut n = value.len();
            while n >= 128 { data.push((n as u8) | 128); n >>= 7; }
            data.push(n as u8); data.extend_from_slice(value);
        }
        let mut blob = b"\xff Go buildinf:".to_vec(); blob.resize(32, 0); blob[15] = 2;
        append(&mut blob, b"go1.26.3");
        let mut framed = vec![0; 16]; framed.extend_from_slice(modules.as_bytes()); framed.extend_from_slice(&[0; 16]);
        append(&mut blob, &framed); blob
    }

    #[test]
    fn reads_dependency_and_does_not_guess_replacements_or_future_versions() {
        let dep = "dep\tgithub.com/containers/gvisor-tap-vsock\tv0.8.9\th1:test\n";
        assert_eq!(decode(&build_blob(dep)).unwrap().default_dial_limit, Some(10));
        let vendor_dep = "dep\tgithub.com/containers/gvisor-tap-vsock\tv0.8.9\t\n";
        assert_eq!(decode(&build_blob(vendor_dep)).unwrap().default_dial_limit, None);
        let patched = decode(&build_blob(&format!("{vendor_dep}build\t-tags=vpnmgr_lima_2_1_2,vpnmgr_gvisor_698_8b4db4a\n"))).unwrap();
        assert_eq!(patched.default_dial_limit, Some(128));
        assert!(patched.gvisor_patch.is_some());
        let replaced = decode(&build_blob(&format!("{dep}=>\t../patched\t(devel)\t\n"))).unwrap();
        assert!(replaced.gvisor_replaced);
        assert_eq!(replaced.default_dial_limit, None);
        assert_eq!(decode(&build_blob(&dep.replace("v0.8.9", "v9.0.0"))).unwrap().default_dial_limit, None);
        assert!(decode(&build_blob(dep)[..40]).is_err());
    }
}
