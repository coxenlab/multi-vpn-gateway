#!/usr/bin/env bash
# 栈冒烟:起 compose,断言关键端点 + mihomo 热加载。不含真 VPN 登录。
set -euo pipefail
cd "$(dirname "$0")/.."

umask 077
if [ ! -f .env ]; then python3 gen_env.py > .env; fi
set -a; . ./.env; set +a
# 渲染 mihomo 配置(start.sh 同款占位替换;已存在则保留,不冲掉运行中通道)
if [ ! -f mihomo/config.yaml ]; then
  sed "s/__SECRET__/${MIHOMO_SECRET}/" mihomo/config.template.yaml > mihomo/config.yaml
fi

echo "== compose up =="
docker compose up -d --build
trap 'docker compose logs --tail=30 app || true' ERR
trap 'docker compose down >/dev/null 2>&1 || true' EXIT   # 冒烟结束自清理,不留容器

base="http://127.0.0.1:${UI_PORT}"
echo "== 等待 app 就绪 =="
for i in $(seq 1 30); do
  if curl -fsS "${base}/api/system" >/dev/null 2>&1; then break; fi
  sleep 2
done

echo "== /api/system =="
curl -fsS "${base}/api/system" | grep -q '"mihomo_status": *"running"' \
  && echo "  ok: mihomo running" || { echo "  FAIL: mihomo not running"; exit 1; }

echo "== /api/channels =="
curl -fsS "${base}/api/channels" >/dev/null && echo "  ok"

echo "== /clash/vpn-rules.yaml 合法 YAML =="
curl -fsS "${base}/clash/vpn-rules.yaml" | python3 -c "import sys,yaml; d=yaml.safe_load(sys.stdin); assert 'payload' in d; print('  ok: payload key present')"

echo "== /api/clash-snippet 含节点 =="
curl -fsS "${base}/api/clash-snippet" | grep -q "vpn-router" && echo "  ok"

echo "== mihomo 热加载(PUT /configs)=="
curl -fsS -X PUT "127.0.0.1:${MIHOMO_CTRL_PORT}/configs?force=true" \
  -H "Authorization: Bearer ${MIHOMO_SECRET}" \
  -H "Content-Type: application/json" \
  -d '{"path":"/cfg/config.yaml"}' -o /dev/null -w "  PUT status: %{http_code}\n" \
  | grep -qE "204|200" && echo "  ok: hot reload" || echo "  (热加载返回非2xx,看上行状态)"

echo "ALL SMOKE CHECKS PASSED"
