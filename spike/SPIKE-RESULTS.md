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
| tun 可用性(ip tuntap add tun0) | 命门#6(usability) | ✅ |
| route_localnet sysctl(aTrust) | 命门#6(aTrust) | ✅ |

## 结论
- [x] GO:全部 ✅ → 投入 Phase 1 全量 Rust 端口,运行时锁定 colima/Lima(vz)+ 开源 Engine。
- [ ] NO-GO:无(无硬失败,podman machine 兜底不需要)。

## 残留风险(Phase 1 需首次验证)

以下事项 spike 未覆盖,Phase 1 首次对真实组件测试时需逐项核实:

1. **真实 VPN 镜像的 tun 可用性**:spike 用 alpine + iproute2 验证了 open()+TUNSETIFF;EC/aTrust/oss 真实镜像的相同 ioctl 在 colima/vz cgroup 规则下的行为尚未证明。
2. **mihomo 配置热加载**:命门#3 的 `PUT /configs?force=true` 未 spike;Phase 1 须对真实 mihomo 容器验证热加载不断连。
3. **镜像平台选择**:spike 传 `platform: None` 让 Docker 自选;aTrust 必须原生 arm64(memory `atrust-needs-native-arm64`),Phase 1 须明确 `platform: linux/arm64`。
4. **reqwest socks5h 探活**:命门#1 的 `socks5h://vpn-{id}:1080` 探活(reqwest socks feature)未 spike;Phase 1 须端到端验证 DNS 在 VPN 侧解析。
5. **exec 并发 drain**:当前 `exec_inject_stdin` 先写完 stdin 再 drain stdout,是串行的;若某 adapter(如 openconnect)在读完 stdin 前先写大量 stdout(banner),可能阻塞——Phase 1 应改为并发 drain。
6. **exec 退出码检查**:`exec_capture`/`exec_inject_stdin` 不检查 exit_code;Phase 1 的 oss_connect 须 inspect_exec 判成败,防止静默失败。
7. **colima 自动 provisioning**:本 spike 假设 colima 已手动启动;自动安装/启动 colima 的流程未 spike。

## bollard 版本备忘
- exec Attached{output,input}:实测可写 stdin = 是
- upload_to_container body 类型(本机 0.17.1 实际签名):plain `bytes::Bytes`(无需 `.into()` 包装)
