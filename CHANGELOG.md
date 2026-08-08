# Changelog

本项目变更记录。格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)。

## [Unreleased]

### Changed / 变更
- 桌面版 noVNC 容器端口不再交给 Docker/lima publish；改由 app 为每条通道持有独立的 loopback SSH 转发，并由登录入口与看门狗按需自愈。
- 项目结构规整:`vpn-manager/` 拍平到仓库根;设计文档移入 `docs/`(`design.md`);README 改为中英双语(`README.md` / `README.zh-CN.md`)。
- 重新授权为 **MIT**(原为私有 / 保留所有权利);新增 `NOTICE` 第三方与闭源软件免责声明。
- 内部开发文档(架构 / 命门 / 贡献说明)集中到 `docs/development.md`。

### Added
- 桌面版新增通道「参与分流」、可逆的全局「全直连」、规则备注与批量保护，以及仅内存生效的自动修复暂停；三级开关统一折叠到同一有效规则集。
- 新增桌面控制端点:`GET|POST /api/routing`、`POST /api/system/self-heal`，并扩展通道与规则 PATCH / 配置导入导出字段。
- 配置导出导入:总览屏顶栏「导出 / 导入配置」;`GET /api/config/export` + `POST /api/config/import`(通道 + 分流规则 + 无头通道注入配置;交互登录密码不导出、导入后重新登录;导入只落库为「已停止」,按需逐个启动;Python 后端与 Rust core 双端等价)。
- 后端分流规则支持 `kind`(domain / ip)+ `enabled`,新增 IP-CIDR、rule-provider 与遥测。
- 前端 5 屏接入真实 API,移除 mock `data.js`。
- Docker 栈冒烟脚本 `tests/smoke.sh`。
- 项目基础文件:README、LICENSE、CONTRIBUTING、.editorconfig、.dockerignore。

### Fixed
- noVNC 退出 lima gRPC 自动转发故障域；停止 / 删除与自动确保串行，避免 SSH 子进程在回收后竞态复活。
- arm64 上交互式 noVNC 登录 + mihomo 路由可用(websockify 自愈、路径尾斜杠)。
- 多处实机 UI 走查修正。

### Notes
- 历史从项目导入(落地方案 + FastAPI 后端 + mihomo)起算;此前未维护版本号。
