use anyhow::{anyhow, Result};

/// 用与 Python cryptography.Fernet 相同的 key 解密其密文。
/// 用 decrypt()(不带 TTL),保证旧 token 不被时间拒绝。
pub fn decrypt_fernet(key: &str, token: &str) -> Result<Vec<u8>> {
    let f = fernet::Fernet::new(key).ok_or_else(|| anyhow!("invalid fernet key"))?;
    f.decrypt(token).map_err(|e| anyhow!("fernet decrypt failed: {e:?}"))
}

/// 占位:证明工程能编译能测。后续任务会替换/扩充本文件。
pub fn spike_ready() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_builds_and_runs() {
        assert!(spike_ready());
    }
}
