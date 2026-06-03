#!/usr/bin/env bash
# 一键启动。所有端口高位随机、只听 127.0.0.1、首次分配后持久化。
set -euo pipefail
cd "$(dirname "$0")"

# 1) 首次运行:生成随机高位端口 + mihomo 密钥,写入 .env(以后保持不变)
[ -f .env ] || python3 gen_env.py > .env
set -a; . ./.env; set +a

# 2) 首次运行:用密钥渲染 mihomo 初始配置(空规则)。已存在则保留(里面有你建好的通道)
if [ ! -f mihomo/config.yaml ]; then
  sed "s/__SECRET__/${MIHOMO_SECRET}/" mihomo/config.template.yaml > mihomo/config.yaml
fi

echo "==> 构建并启动(全 Docker,本机零新增依赖)..."
docker compose up -d --build

cat <<EOF

==> 起来了。端口都是高位随机、只听 127.0.0.1:
    管理界面:        http://127.0.0.1:${UI_PORT}
    mihomo 分流端口:  127.0.0.1:${MIHOMO_PORT}   (给你的 Clash 接,见界面里"Clash 配置"按钮)
    mihomo 控制台:    127.0.0.1:${MIHOMO_CTRL_PORT}

停止:  docker compose down
清理某个 VPN 容器: 在界面点删除,或 docker rm -f vpn-<id>
EOF
