# 开发说明

> 本文件是给贡献者 / 接手者的最小约定。架构、命门与现状见 [`docs/development.md`](./docs/development.md);完整设计意图见 [`docs/design.md`](./docs/design.md)。

## 环境

- macOS / 一台常开小主机,装 Docker(已实测 Docker 29.3.1 / API 1.54 / arm64)。
- 后端依赖只在 Docker 镜像里,系统 Python 没装 fastapi——**本机无法直接 `uvicorn` 起 app**,要起整栈走 compose。

## 跑起来

```bash
./start.sh                              # 全栈:gen_env → 渲染 mihomo 配置 → compose up
# 管理界面 http://127.0.0.1:${UI_PORT}(端口见 .env);停止 docker compose down
```

只改前端、不起后端:

```bash
cd app/static && python3 -m http.server 8080
```

## 测试

单元测试用 pytest,依赖独立于运行镜像(`tests/conftest.py` 在 import 前注入测试用 env,SQLite 落临时目录,manager 的容器 / 探活等副作用全 monkeypatch 掉),从仓库根运行:

```bash
pip install -r tests/requirements-dev.txt
pytest tests/                           # adapters / api / clash / dockerhub / manager / registry / store
```

栈冒烟(需 Docker,起真 compose、断言关键端点 + mihomo 热加载,不含真 VPN 登录):

```bash
./tests/smoke.sh
```

## 约定

- **改 API 必须同步改 README 的 API 表**,并以 `app/main.py` 为唯一事实源。
- 提交信息沿用 `type(scope): 摘要`(feat / fix / test / docs / chore)。
- 前端设计系统见 [`docs/development.md`](./docs/development.md) 的「前端设计系统」一节,保持 Neutral Modern,不要 AI-slop。

## 绝不能破坏的命门(详见 [`docs/development.md`](./docs/development.md))

1. 登录成功的唯一判据 = 后端 SOCKS5 探活,**不是**「VNC 连上了」。
2. 配置热加载、绝不断连(`PUT /configs?force=true`,不重启 mihomo)。
3. 所有 host 端口只绑 `127.0.0.1`,永不 `0.0.0.0`。
4. 凭据 Fernet 加密落库,接口永不回传密文;`master.key` 权限 0600。
