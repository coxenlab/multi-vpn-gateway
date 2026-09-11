#!/usr/bin/env bash
# 静态检查和隔离单测入口;真实 VM / VPN / 浏览器验收另行执行。
set -euo pipefail
VERIFY_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$VERIFY_ROOT"
if [[ ! -x .venv/bin/python ]]; then
  echo '缺少 .venv；请按 docs/development.md 安装测试依赖。' >&2
  exit 1
fi
for tool in cargo node; do
  command -v "$tool" >/dev/null || { echo "缺少 $tool" >&2; exit 1; }
done
# 不继承启动 app 的数据/配置/daemon 环境；各测试仍应使用自己的临时 Config。
VERIFY_DATA="$(mktemp -d "${TMPDIR:-/tmp}/vpnmgr-verify.XXXXXX")"
trap 'rm -rf -- "$VERIFY_DATA"' EXIT
export DATA_DIR="$VERIFY_DATA"
export MIHOMO_CONFIG_PATH="$VERIFY_DATA/mihomo.yaml"
export DOCKER_HOST="unix://$VERIFY_DATA/absent-docker.sock"
export VPNMGR_DEV_MODE=1
export VPNMGR_VM_PROFILE=vpnmgr-verify
.venv/bin/python -m pytest tests -q
cargo test --locked --offline --manifest-path desktop/core/Cargo.toml
cargo test --locked --offline --manifest-path desktop/helper/Cargo.toml
cargo clippy --locked --offline --manifest-path desktop/core/Cargo.toml --all-targets -- -D warnings
cargo clippy --locked --offline --manifest-path desktop/helper/Cargo.toml --all-targets -- -D warnings
cargo check --locked --offline --manifest-path desktop/app/Cargo.toml
.venv/bin/python - <<'PY'
from html.parser import HTMLParser
from pathlib import Path
import ast
import subprocess

class Scripts(HTMLParser):
    def __init__(self):
        super().__init__(); self.active = None; self.code = []; self.scripts = []
    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if tag == 'script' and 'src' not in attrs and attrs.get('type', '') in ('', 'text/javascript', 'module'):
            self.active = attrs.get('type', ''); self.code = []
    def handle_data(self, data):
        if self.active is not None: self.code.append(data)
    def handle_endtag(self, tag):
        if tag == 'script' and self.active is not None:
            self.scripts.append((self.active, ''.join(self.code))); self.active = None

for path in Path('desktop/app').glob('*.py'):
    ast.parse(path.read_text(), filename=str(path))
for path in sorted(Path('app/static/js').rglob('*.js')):
    result = subprocess.run(['node', '--check', '--input-type=module'], input=path.read_text(), text=True, capture_output=True)
    if result.returncode:
        raise SystemExit(f'{path}: {result.stderr}')
for path in sorted(Path('app/static').glob('*.html')):
    parser = Scripts(); parser.feed(path.read_text())
    for kind, code in parser.scripts:
        command = ['node', '--check'] + (['--input-type=module'] if kind == 'module' else [])
        result = subprocess.run(command, input=code, text=True, capture_output=True)
        if result.returncode:
            raise SystemExit(f'{path}: {result.stderr}')
for path in [Path('verify.sh'), Path('start.sh'), *Path('desktop/app').glob('*.sh'), *Path('desktop/native').glob('*.sh')]:
    subprocess.run(['bash', '-n', str(path)], check=True)
print('JavaScript / inline scripts / shell syntax passed')
PY

# macOS 原生壳的编译检查不启动界面或 VM；没有远端 Swift Package 依赖。
if [[ "$(uname -s)" == Darwin && -f desktop/native/Package.swift ]]; then
  command -v swift >/dev/null || { echo '缺少 Swift 工具链' >&2; exit 1; }
  swift build --package-path desktop/native -Xswiftc -warnings-as-errors
fi
