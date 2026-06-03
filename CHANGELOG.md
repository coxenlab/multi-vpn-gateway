# Changelog

本项目变更记录。格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)。

## [Unreleased]

### Changed / 变更
- 项目结构规整:`vpn-manager/` 拍平到仓库根;设计文档移入 `docs/`(`design.md`);README 改为中英双语(`README.md` / `README.zh-CN.md`)。
- 重新授权为 **MIT**(原为私有 / 保留所有权利);新增 `NOTICE` 第三方与闭源软件免责声明。
- 内部开发文档(架构 / 命门 / 贡献说明)集中到 `docs/development.md`。

### Added
- 后端分流规则支持 `kind`(domain / ip)+ `enabled`,新增 IP-CIDR、rule-provider 与遥测。
- 前端 5 屏接入真实 API,移除 mock `data.js`。
- Docker 栈冒烟脚本 `tests/smoke.sh`。
- 项目基础文件:README、LICENSE、CONTRIBUTING、.editorconfig、.dockerignore。

### Fixed
- arm64 上交互式 noVNC 登录 + mihomo 路由可用(websockify 自愈、路径尾斜杠)。
- 多处实机 UI 走查修正。

### Notes
- 历史从项目导入(落地方案 + FastAPI 后端 + mihomo)起算;此前未维护版本号。
