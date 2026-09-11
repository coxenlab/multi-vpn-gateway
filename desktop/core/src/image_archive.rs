//! Docker save archive inspection: bounded reads, no extraction, content and architecture verification.
use anyhow::{anyhow, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Component, Path},
};

pub const MAX_ARCHIVE: u64 = 12 * 1024 * 1024 * 1024;
const MAX_EXPANDED: u64 = 32 * 1024 * 1024 * 1024;
const MAX_METADATA: usize = 16 * 1024 * 1024;
#[derive(Clone, Debug, Serialize)]
pub struct Image {
    pub tags: Vec<String>,
    pub id: String,
    pub architecture: String,
    pub adapters: Vec<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct Preview {
    pub images: Vec<Image>,
    pub bytes: u64,
    pub sha256: String,
}
#[derive(Deserialize, Serialize)]
struct ManifestEntry {
    #[serde(rename = "Config")]
    config: String,
    #[serde(rename = "RepoTags")]
    tags: Vec<String>,
    #[serde(rename = "Layers")]
    layers: Vec<String>,
}
struct Entry {
    hash: String,
    content_hash: String,
    json: Option<Vec<u8>>,
}
struct Meter<R> {
    reader: R,
    hash: Sha256,
    bytes: u64,
    limit: u64,
}
impl<R: Read> Read for Meter<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.reader.read(buf)?;
        self.bytes += n as u64;
        if self.bytes > self.limit {
            return Err(std::io::Error::other("镜像展开后超过大小限制"));
        }
        self.hash.update(&buf[..n]);
        Ok(n)
    }
}
fn reader<R: Read + 'static>(r: R) -> Result<Box<dyn Read>> {
    let mut buf = BufReader::new(r);
    if buf.fill_buf()?.starts_with(&[0x1f, 0x8b]) {
        Ok(Box::new(flate2::bufread::MultiGzDecoder::new(buf)))
    } else {
        Ok(Box::new(buf))
    }
}
fn safe_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\\')
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}
#[cfg(test)]
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn approved(tag: &str) -> Result<Vec<String>> {
    let mut matches = Vec::new();
    for adapter in crate::registry::list_adapters()? {
        let spec = crate::registry::get(&adapter.key)?;
        // Match the exact app image, or a declared versioned repository with a concrete tag.
        let version_match = spec.versioned
            && spec.image.contains("{version}")
            && tag
                .strip_prefix(&spec.image.replace("{version}", ""))
                .is_some_and(|version| {
                    !version.is_empty()
                        && version.len() <= 128
                        && version
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
                });
        if tag == spec.image || version_match {
            matches.push(adapter.label);
        }
    }
    ensure!(
        !matches.is_empty(),
        "镜像 {tag} 不在本应用支持的 VPN 模板清单内"
    );
    Ok(matches)
}

pub fn inspect(path: &Path, architecture: &str) -> Result<Preview> {
    inspect_file(&mut File::open(path)?, architecture)
}
pub fn inspect_file(source: &mut File, architecture: &str) -> Result<Preview> {
    Ok(inspect_contents(source, architecture)?.0)
}
fn inspect_contents(
    source: &mut File,
    architecture: &str,
) -> Result<(Preview, Vec<ManifestEntry>)> {
    let bytes = source.metadata()?.len();
    ensure!(
        bytes > 0 && bytes <= MAX_ARCHIVE,
        "镜像归档为空或超过 12 GiB"
    );
    source.seek(SeekFrom::Start(0))?;
    let mut hash = Sha256::new();
    let mut buf = [0; 65536];
    loop {
        let n = source.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    let sha256 = format!("{:x}", hash.finalize());
    source.seek(SeekFrom::Start(0))?;
    let expanded = Meter {
        reader: reader(source.try_clone()?)?,
        hash: Sha256::new(),
        bytes: 0,
        limit: MAX_EXPANDED,
    };
    let mut archive = tar::Archive::new(expanded);
    let mut entries = BTreeMap::new();
    let mut metadata_size = 0;
    let mut expanded_layers = 0;
    for (index, entry) in archive
        .entries()
        .context("不是受支持的 Docker save tar/tar.gz 归档")?
        .enumerate()
    {
        let mut entry = entry?;
        ensure!(index < 4096, "归档文件数量过多");
        let path = entry
            .path()?
            .to_str()
            .ok_or_else(|| anyhow!("归档路径无法识别"))?
            .to_owned();
        ensure!(safe_path(path.trim_end_matches('/')), "归档含不安全路径");
        if entry.header().entry_type().is_dir() {
            continue;
        }
        ensure!(
            entry.header().entry_type().is_file(),
            "归档外层不能包含链接或设备文件"
        );
        ensure!(!entries.contains_key(&path), "归档含重复路径");
        let is_json = path.ends_with(".json") || path == "repositories";
        let mut content = Vec::new();
        let mut raw = Meter {
            reader: &mut entry,
            hash: Sha256::new(),
            bytes: 0,
            limit: MAX_EXPANDED,
        };
        let mut content_hash = Sha256::new();
        {
            let mut buffered = BufReader::new(&mut raw);
            let gzip = buffered.fill_buf()?.starts_with(&[0x1f, 0x8b]);
            ensure!(!is_json || !gzip, "元数据不能被单独压缩");
            let mut data: Box<dyn Read + '_> = if gzip {
                Box::new(flate2::bufread::MultiGzDecoder::new(buffered))
            } else {
                Box::new(buffered)
            };
            loop {
                let n = data.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                expanded_layers += n as u64;
                ensure!(expanded_layers <= MAX_EXPANDED, "镜像层展开超过 32 GiB");
                content_hash.update(&buf[..n]);
                if is_json || path.starts_with("blobs/sha256/") && entry_size_candidate(&content, n)
                {
                    // OCI config blobs have no suffix. Keep only small objects, bounded in aggregate.
                    if content.len() + n <= 2 * 1024 * 1024 {
                        content.extend_from_slice(&buf[..n]);
                    } else {
                        content.clear();
                    }
                }
            }
        }
        if is_json {
            ensure!(raw.bytes <= 2 * 1024 * 1024, "镜像元数据过大");
        }
        let json = if (is_json || raw.bytes <= 2 * 1024 * 1024)
            && serde_json::from_slice::<serde_json::Value>(&content).is_ok()
        {
            metadata_size += content.len();
            ensure!(metadata_size <= MAX_METADATA, "镜像元数据总量过大");
            Some(content)
        } else {
            None
        };
        let entry_hash = format!("{:x}", raw.hash.finalize());
        if let Some(blob) = path.strip_prefix("blobs/sha256/") {
            ensure!(blob == entry_hash, "镜像 blob 校验不一致");
        }
        entries.insert(
            path,
            Entry {
                hash: entry_hash,
                content_hash: format!("{:x}", content_hash.finalize()),
                json,
            },
        );
    }
    // Force gzip checksum/truncation validation, rejecting data hidden after the tar terminator.
    let mut remainder = archive.into_inner();
    loop {
        let n = remainder.read(&mut buf)?;
        if n == 0 {
            break;
        }
        ensure!(buf[..n].iter().all(|b| *b == 0), "归档尾部含额外数据");
    }
    let manifest = entries
        .get("manifest.json")
        .and_then(|e| e.json.as_ref())
        .ok_or_else(|| anyhow!("缺少 Docker save manifest；VM 磁盘和客户端安装器请使用对应入口"))?;
    let manifest: Vec<ManifestEntry> = serde_json::from_slice(manifest)?;
    ensure!(
        !manifest.is_empty() && manifest.len() <= 16,
        "归档镜像数量须为 1–16"
    );
    let mut tags_seen = BTreeSet::new();
    let mut images = Vec::new();
    for image in &manifest {
        ensure!(
            !image.tags.is_empty() && image.tags.len() <= 32,
            "镜像缺少标签或标签过多"
        );
        let mut adapters = BTreeSet::new();
        for tag in &image.tags {
            ensure!(tags_seen.insert(tag.clone()), "归档含重复镜像标签");
            adapters.extend(approved(tag)?);
        }
        let config = entries
            .get(&image.config)
            .ok_or_else(|| anyhow!("镜像配置缺失"))?;
        ensure!(
            image.config == format!("{}.json", config.hash)
                || image.config == format!("blobs/sha256/{}", config.hash),
            "镜像配置摘要不一致"
        );
        let config: serde_json::Value = serde_json::from_slice(
            config
                .json
                .as_ref()
                .ok_or_else(|| anyhow!("镜像配置无效或过大"))?,
        )?;
        ensure!(
            config["os"] == "linux" && config["architecture"] == architecture,
            "镜像系统或架构不匹配，当前需要 linux/{architecture}"
        );
        let layers = config["rootfs"]["diff_ids"]
            .as_array()
            .ok_or_else(|| anyhow!("镜像层摘要缺失"))?;
        ensure!(layers.len() == image.layers.len(), "镜像层数量不一致");
        for (name, expected) in image.layers.iter().zip(layers) {
            let entry = entries.get(name).ok_or_else(|| anyhow!("镜像层缺失"))?;
            ensure!(
                expected.as_str() == Some(&format!("sha256:{}", entry.content_hash)),
                "镜像层内容校验失败"
            );
        }
        let id = format!("sha256:{}", entries[&image.config].hash);
        images.push(Image {
            tags: image.tags.clone(),
            id,
            architecture: architecture.into(),
            adapters: adapters.into_iter().collect(),
        });
    }
    if let Some(repos) = entries.get("repositories") {
        let repos: BTreeMap<String, BTreeMap<String, String>> = serde_json::from_slice(
            repos
                .json
                .as_ref()
                .ok_or_else(|| anyhow!("仓库标签元数据无效"))?,
        )?;
        for (repo, tags) in repos {
            for tag in tags.keys() {
                ensure!(
                    tags_seen.contains(&format!("{repo}:{tag}")),
                    "归档包含未预览的附加标签"
                );
            }
        }
    }
    Ok((
        Preview {
            images,
            bytes,
            sha256,
        },
        manifest,
    ))
}
fn entry_size_candidate(content: &[u8], n: usize) -> bool {
    content.len() + n <= 2 * 1024 * 1024
}

/// Emit only the validated Docker-save manifest/config/layers. OCI indexes or extra repository
/// metadata must never cause the daemon to load tags/images that were absent from the preview.
pub fn prepare(
    source: &mut File,
    destination: File,
    architecture: &str,
) -> Result<(File, Preview)> {
    let (preview, manifest) = inspect_contents(source, architecture)?;
    let mut required = BTreeSet::new();
    for image in &manifest {
        required.insert(image.config.clone());
        required.extend(image.layers.clone());
    }
    let mut output = tar::Builder::new(destination);
    let bytes = serde_json::to_vec(&manifest)?;
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o600);
    header.set_cksum();
    output.append_data(&mut header, "manifest.json", bytes.as_slice())?;
    source.seek(SeekFrom::Start(0))?;
    let mut archive = tar::Archive::new(reader(source.try_clone()?)?);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if !required.remove(path.to_str().ok_or_else(|| anyhow!("归档路径无法识别"))?) {
            continue;
        }
        let mut header = tar::Header::new_gnu();
        header.set_size(entry.size());
        header.set_mode(0o600);
        header.set_cksum();
        output.append_data(&mut header, &path, &mut entry)?;
    }
    ensure!(required.is_empty(), "校验后归档内容发生变化");
    let mut file = output.into_inner()?;
    file.sync_all()?;
    file.seek(SeekFrom::Start(0))?;
    Ok((file, preview))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;
    pub(crate) fn fixture(arch: &str, tag: &str, corrupt: bool) -> Vec<u8> {
        let layer = b"synthetic layer";
        let config = serde_json::to_vec(&serde_json::json!({"os":"linux","architecture":arch,"rootfs":{"type":"layers","diff_ids":[format!("sha256:{}",digest(layer))]}})).unwrap();
        let name = format!("{}.json", digest(&config));
        let manifest = serde_json::to_vec(
            &serde_json::json!([{"Config":name,"RepoTags":[tag],"Layers":["layer/layer.tar"]}]),
        )
        .unwrap();
        let mut archive = tar::Builder::new(Vec::new());
        for (name, bytes) in [
            (name.as_str(), config.as_slice()),
            ("manifest.json", manifest.as_slice()),
            (
                "layer/layer.tar",
                if corrupt {
                    b"changed".as_slice()
                } else {
                    layer.as_slice()
                },
            ),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o600);
            header.set_cksum();
            archive.append_data(&mut header, name, bytes).unwrap();
        }
        archive.into_inner().unwrap()
    }
    #[test]
    fn validates_archive_and_gzip_without_extraction() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("image.tar");
        let bytes = fixture("arm64", "vpnmgr/oss-vpn:latest", false);
        std::fs::write(&file, &bytes).unwrap();
        let result = inspect(&file, "arm64").unwrap();
        assert_eq!(result.images.len(), 1);
        assert_eq!(result.sha256, digest(&bytes));
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(&bytes).unwrap();
        std::fs::write(&file, gzip.finish().unwrap()).unwrap();
        assert_eq!(
            inspect(&file, "arm64").unwrap().images[0].id,
            result.images[0].id
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
    #[test]
    fn strips_unreviewed_oci_metadata_and_rejects_unsafe_outer_archives() {
        let original = fixture("arm64", "vpnmgr/oss-vpn:latest", false);
        let extra = |kind: &str| {
            let mut archive = tar::Builder::new(Vec::new());
            for entry in tar::Archive::new(original.as_slice()).entries().unwrap() {
                let mut entry = entry.unwrap();
                let path = entry.path().unwrap().into_owned();
                let mut header = entry.header().clone();
                archive.append_data(&mut header, path, &mut entry).unwrap();
            }
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o600);
            match kind {
                "link" => {
                    header.set_entry_type(tar::EntryType::Symlink);
                    header.set_size(0);
                    header.set_link_name("/outside").unwrap();
                    header.set_cksum();
                    archive
                        .append_data(&mut header, "linked", std::io::empty())
                        .unwrap();
                }
                "traversal" => {
                    header.set_size(0);
                    header.as_mut_bytes()[..10].copy_from_slice(b"../outside");
                    header.set_cksum();
                    archive.append(&header, std::io::empty()).unwrap();
                }
                "duplicate" => {
                    header.set_size(2);
                    header.set_cksum();
                    archive
                        .append_data(&mut header, "manifest.json", b"[]".as_slice())
                        .unwrap();
                }
                _ => {
                    let bytes=br#"{"manifests":[{"annotations":{"io.containerd.image.name":"foreign/image:latest"}}]}"#;
                    header.set_size(bytes.len() as u64);
                    header.set_cksum();
                    archive
                        .append_data(&mut header, "index.json", bytes.as_slice())
                        .unwrap();
                }
            }
            archive.into_inner().unwrap()
        };
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(&extra("index")).unwrap();
        let (mut canonical, preview) =
            prepare(&mut file, tempfile::tempfile().unwrap(), "arm64").unwrap();
        assert_eq!(preview.images[0].tags, ["vpnmgr/oss-vpn:latest"]);
        let names = tar::Archive::new(&mut canonical)
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 3);
        assert!(!names.iter().any(|p| p == Path::new("index.json")));
        assert_eq!(
            inspect_file(&mut canonical, "arm64").unwrap().images[0].id,
            preview.images[0].id
        );
        for kind in ["link", "traversal", "duplicate"] {
            let file = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(file.path(), extra(kind)).unwrap();
            assert!(inspect(file.path(), "arm64").is_err(), "{kind}");
        }
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(&original).unwrap();
        let mut compressed = gzip.finish().unwrap();
        compressed.truncate(compressed.len() - 4);
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), compressed).unwrap();
        assert!(inspect(file.path(), "arm64").is_err());
    }
    #[test]
    fn rejects_corruption_wrong_arch_unknown_tags_and_other_packages() {
        let file = tempfile::NamedTempFile::new().unwrap();
        for bytes in [
            fixture("arm64", "vpnmgr/oss-vpn:latest", true),
            fixture("amd64", "vpnmgr/oss-vpn:latest", false),
            fixture("arm64", "foreign/image:latest", false),
            b"not a Docker archive".to_vec(),
        ] {
            std::fs::write(file.path(), bytes).unwrap();
            assert!(inspect(file.path(), "arm64").is_err());
        }
    }
}
