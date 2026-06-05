# Phase 0 Spike 结论(2026-06-05)

环境:macOS 26.4 / Apple M3 Max / colima 0.10.3(vz)/ VM Docker Engine 29.5.2 / bollard 0.17.1 / fernet 0.2.2

| 验收门 | 命门/风险 | 结果 |
|---|---|---|
| Fernet 零迁移解密 | 命门#5(加密层) | ✅ |
| bollard↔VM DOCKER_HOST 连接 | 架构契约 | ✅ |
| exec 可写 stdin 注入(不进 argv) | 命门#5 / 验收门1 | ✅ |
| put_archive 落文件 | byo / 验收门2 | ✅ |
| 用户 bridge 按名解析 | mihomo→vpn-{id} | ✅ |
| --device tun + cap + sysctl | 命门#6 | ✅ |

## 结论
- [x] GO:全部 ✅ → 投入 Phase 1 全量 Rust 端口,运行时锁定 colima/Lima(vz)+ 开源 Engine。
- [ ] 局部风险:无(全部门通过,无 caveat)。
- [ ] NO-GO:无(无硬失败,podman machine 兜底不需要)。

## bollard 版本备忘
- exec Attached{output,input}:实测可写 stdin = 是
- upload_to_container body 类型(本机 0.17.1 实际签名):plain `bytes::Bytes`(无需 `.into()` 包装)
