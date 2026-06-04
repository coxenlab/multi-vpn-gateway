"""Docker 环境/容器体检引擎 + 镜像类自动修复。

检查函数一律接收 docker 客户端作为参数(依赖注入,便于单测),且永不抛异常:
内部任何错误都转成 warn/fail 的 CheckResult。镜像源 P1 硬编码(P2 切到 mirrors 表)。
"""
import threading
import uuid
import requests
import docker
import registry
import dockerhub

# P1 硬编码镜像源(按顺序探测可达再拉);P2 改为读 store.mirrors 表
DEFAULT_MIRRORS = ["docker.1ms.run", "hub.rat.dev"]

# 自建镜像的本地构建上下文(镜像名前缀 → 仓库内构建目录)
_BUILD_CONTEXT = {"vpnmgr/oss-vpn": "images/oss", "vpnmgr/byo-desktop": "images/byo"}

# 基础设施镜像(定义在 docker-compose,不在 adapters):分流底座 + 管理后端
INFRA_IMAGES = [
    {"image": "metacubex/mihomo:latest", "kind": "pull", "title": "mihomo 分流底座",
     "arch": ["amd64", "arm64"]},
    {"image": "app", "kind": "compose", "title": "管理后端(FastAPI)",
     "build_context": "app", "arch": []},
]


def resolve_image(vpn_type, version=None):
    """按 vpn_type 解析最终镜像名(替换 {version} 占位)。未知类型抛 KeyError。"""
    spec = registry.get(vpn_type)
    image = spec["image"]
    if "{version}" in image:
        image = image.format(version=version or "7.6.3")
    return image


def known_repos():
    """所有适配器 + 基础设施声明的镜像 repo(去掉 tag/占位),供 fix 端点做白名单校验。"""
    repos = set()
    for spec in registry.list_adapters():
        img = registry.get(spec["key"])["image"]
        repo = img.split(":", 1)[0].replace("{version}", "").rstrip(":")
        repos.add(repo)
    for inf in INFRA_IMAGES:
        if inf["kind"] == "pull":
            repos.add(inf["image"].split(":", 1)[0])
    return repos


def is_buildable(image):
    """vpnmgr/* 是自建镜像(镜像源上没有),应本地构建而非拉取。"""
    return image.split(":", 1)[0] in _BUILD_CONTEXT


def _split_image(full):
    """拆镜像串 → (repo, tag_or_None, versioned, image_field, display)。
    versioned(含 {version})→ tag=None、image_field=repo;否则 image_field=完整名、tag 默认 latest。"""
    if "{version}" in full:
        repo = full.split(":", 1)[0]
        return repo, None, True, repo, full
    repo, _, tag = full.partition(":")
    return repo, (tag or "latest"), False, full, full


def _image_present(dc, image):
    """本机是否已有该镜像。ImageNotFound→False,其它异常→None(永不抛,沿用 preflight 风格)。"""
    try:
        dc.images.get(image)
        return True
    except docker.errors.ImageNotFound:
        return False
    except Exception:
        return None


def image_inventory(dc, host_arch, mirrors):
    """汇总本系统全部镜像 + 下载/构建元信息。返回 {host_arch, mirrors, images:[...]}。
    VPN 镜像从 registry 去重推导(oss 8 协议并成 1 条),infra 来自 INFRA_IMAGES。"""
    entries = {}
    order = []

    for spec in registry.list_adapters():
        full = registry.get(spec["key"])["image"]
        repo, tag, versioned, image_field, display = _split_image(full)
        key = repo if versioned else image_field
        if key not in entries:
            entries[key] = {
                "image": image_field, "display": display, "repo": repo, "tag": tag,
                "kind": "build" if is_buildable(image_field) else "pull",
                "role": "vpn", "title": spec["label"], "used_by": [],
                "arch": list(spec.get("arch", [])), "versioned": versioned,
                "build_context": _BUILD_CONTEXT.get(repo),
                "versions": [], "present": None,
                "_fallback": spec.get("fallback_versions", []),
            }
            order.append(key)
        e = entries[key]
        e["used_by"].append(spec["label"])
        for a in spec.get("arch", []):
            if a not in e["arch"]:
                e["arch"].append(a)

    for inf in INFRA_IMAGES:
        repo, tag, _, image_field, display = _split_image(inf["image"])
        entries[image_field] = {
            "image": image_field, "display": display, "repo": repo, "tag": tag,
            "kind": inf["kind"], "role": "infra", "title": inf["title"], "used_by": [],
            "arch": list(inf.get("arch", [])), "versioned": False,
            "build_context": inf.get("build_context"),
            "versions": [], "present": None, "_fallback": [],
        }
        order.append(image_field)

    for key in order:
        e = entries[key]
        fb = e.pop("_fallback")
        if e["versioned"]:
            e["versions"] = dockerhub.versions(e["repo"], host_arch, fb)
        elif e["kind"] != "compose":
            if e["kind"] == "pull":
                e["versions"] = [{"tag": e["tag"], "arch": e["arch"], "usable_here": True}]
            e["present"] = _image_present(dc, e["image"])

    return {"host_arch": host_arch, "mirrors": list(mirrors),
            "images": [entries[k] for k in order]}


def _result(id, layer, title, status, detail="", fix=None):
    r = {"id": id, "layer": layer, "title": title, "status": status, "detail": detail}
    if fix:
        r["fix"] = fix
    return r


def check_docker_daemon(dc):
    try:
        dc.ping()
        return _result("docker_daemon", "引擎", "Docker 守护进程可达", "pass")
    except Exception as e:
        return _result(
            "docker_daemon", "引擎", "Docker 守护进程可达", "fail",
            f"无法连接 Docker:{type(e).__name__}: {e}",
            fix={"kind": "tutorial", "action": "install_docker",
                 "label": "查看安装/启动 Docker 教程"},
        )


def check_image_present(dc, image):
    try:
        dc.images.get(image)
        return _result("image_present", "镜像", "目标镜像本地就绪", "pass", image)
    except docker.errors.ImageNotFound:
        if is_buildable(image):
            ctx = _BUILD_CONTEXT[image.split(":", 1)[0]]
            return _result(
                "image_present", "镜像", "目标镜像本地就绪", "fail",
                f"自建镜像未构建。请在仓库根执行:docker build -t {image} {ctx}",
                fix={"kind": "none"},
            )
        return _result(
            "image_present", "镜像", "目标镜像本地就绪", "fail",
            f"本地缺少镜像 {image},起容器会失败(自动拉取可能因 Docker Hub 网络不通而失败)",
            fix={"kind": "auto", "action": "pull_image",
                 "label": "走国内镜像源拉取", "params": {"image": image}},
        )
    except Exception as e:
        return _result("image_present", "镜像", "目标镜像本地就绪", "warn",
                       f"检查出错:{type(e).__name__}: {e}")


def check_image_arch_match(dc, image, host_arch):
    try:
        img = dc.images.get(image)
    except docker.errors.ImageNotFound:
        return _result("image_arch_match", "镜像", "镜像架构匹配宿主", "skip",
                       "镜像就绪后再检测架构")
    except Exception as e:
        return _result("image_arch_match", "镜像", "镜像架构匹配宿主", "warn",
                       f"检查出错:{type(e).__name__}: {e}")
    arch = (img.attrs or {}).get("Architecture") or ""
    if not arch:
        return _result("image_arch_match", "镜像", "镜像架构匹配宿主", "warn",
                       "无法判定本地镜像架构(多架构存储下可能为空),起容器后留意是否走模拟")
    if arch == host_arch:
        return _result("image_arch_match", "镜像", "镜像架构匹配宿主", "pass",
                       f"{arch} 原生")
    if is_buildable(image):
        return _result("image_arch_match", "镜像", "镜像架构匹配宿主", "warn",
                       f"自建镜像架构 {arch} ≠ 宿主 {host_arch},建议本地重建")
    return _result(
        "image_arch_match", "镜像", "镜像架构匹配宿主", "fail",
        f"本地镜像是 {arch},宿主是 {host_arch} → 会走模拟(如 aTrust 核心会崩)",
        fix={"kind": "auto", "action": "pull_image",
             "label": f"拉取 {host_arch} 版并重打标签",
             "params": {"image": image, "arch": host_arch}},
    )


def check_vpn_network(dc, vpn_net):
    try:
        dc.networks.get(vpn_net)
        return _result("vpn_network", "运行条件", "VPN docker 网络存在", "pass", vpn_net)
    except docker.errors.NotFound:
        return _result(
            "vpn_network", "运行条件", "VPN docker 网络存在", "fail",
            f"docker 网络 {vpn_net} 不存在,容器无法接入",
            fix={"kind": "auto", "action": "create_network",
                 "label": "创建该网络", "params": {"name": vpn_net}},
        )
    except Exception as e:
        return _result("vpn_network", "运行条件", "VPN docker 网络存在", "warn",
                       f"检查出错:{type(e).__name__}: {e}")


def check_dev_net_tun(dc, image, image_ok):
    """用目标镜像跑一次极小探针测 /dev/net/tun(镜像未就绪则跳过)。warn 级、非阻断。"""
    if not image_ok:
        return _result("dev_net_tun", "运行条件", "/dev/net/tun 可用", "skip",
                       "镜像就绪后检测")
    try:
        dc.containers.run(
            image, entrypoint=["/bin/sh", "-c", "test -c /dev/net/tun"],
            devices=["/dev/net/tun:/dev/net/tun:rwm"],
            remove=True, detach=False, network_mode="none",
        )
        return _result("dev_net_tun", "运行条件", "/dev/net/tun 可用", "pass")
    except docker.errors.ContainerError:
        return _result("dev_net_tun", "运行条件", "/dev/net/tun 可用", "warn",
                       "容器内未见 /dev/net/tun,VPN 隧道可能起不来")
    except Exception as e:
        return _result("dev_net_tun", "运行条件", "/dev/net/tun 可用", "warn",
                       f"无法判定(尽力而为):{type(e).__name__}: {e}")


def check_disk_space(dc):
    """信息性:报 Docker 镜像层占用(宿主可用空间在 macOS 上不可靠,故只提示)。"""
    try:
        gb = (dc.df().get("LayersSize", 0)) / 1024**3
        return _result("disk_space", "运行条件", "磁盘空间", "pass",
                       f"Docker 镜像层已占用约 {gb:.1f} GB;每个 VPN 镜像 1.5–5GB,注意留足空间")
    except Exception as e:
        return _result("disk_space", "运行条件", "磁盘空间", "skip",
                       f"无法读取:{type(e).__name__}: {e}")


def check_docker_version(dc):
    try:
        v = dc.version().get("Version", "?")
        return _result("docker_version", "引擎", "Docker 版本", "pass", f"Docker {v}")
    except Exception as e:
        return _result("docker_version", "引擎", "Docker 版本", "warn",
                       f"读取失败:{type(e).__name__}: {e}")


def check_mirror_reachable(mirrors):
    for h in mirrors or []:
        if _mirror_reachable(h):
            return _result("mirror_reachable", "镜像", "国内镜像源可达", "pass", f"{h} 可达")
    return _result("mirror_reachable", "镜像", "国内镜像源可达", "warn",
                   "配置的镜像源都不可达,自动拉取可能失败",
                   fix={"kind": "tutorial", "action": "switch_registry_mirror",
                        "label": "查看切换 Docker 国内源教程"})


def check_mihomo(alive):
    return (_result("mihomo_health", "分流底座", "mihomo 分流实例", "pass", "running")
            if alive else
            _result("mihomo_health", "分流底座", "mihomo 分流实例", "warn",
                    "mihomo 未运行,通道起来了也不会分流"))


_SEVERITY = {"pass": 0, "skip": 0, "warn": 1, "fail": 2}


def run_checks(dc, vpn_type, version, host_arch=None, vpn_net=None,
               scope="preflight", mirrors=None, mihomo_alive=None):
    """跑 P1 检查集(daemon/image_present/arch/network/tun/disk),返回聚合结果。
    host_arch/vpn_net 默认从 registry/manager 取(显式传入便于测试)。"""
    if host_arch is None:
        host_arch = registry.host_arch()
    if vpn_net is None:
        import manager
        vpn_net = manager.VPN_NET

    image = resolve_image(vpn_type, version) if vpn_type else None
    checks = []

    daemon = check_docker_daemon(dc)
    checks.append(daemon)
    if daemon["status"] == "fail":
        # 守护进程不可达:其余依赖项一律 skip,避免一墙红字
        for cid, title in [("image_present", "目标镜像本地就绪"),
                           ("image_arch_match", "镜像架构匹配宿主"),
                           ("vpn_network", "VPN docker 网络存在"),
                           ("dev_net_tun", "/dev/net/tun 可用"),
                           ("disk_space", "磁盘空间")]:
            checks.append(_result(cid, "—", title, "skip", "Docker 不可达,跳过"))
        return _aggregate(checks, host_arch, image)

    if image:
        present = check_image_present(dc, image)
        checks.append(present)
        checks.append(check_image_arch_match(dc, image, host_arch))
        image_ok = present["status"] == "pass"
    else:
        checks.append(_result("image_present", "镜像", "目标镜像本地就绪", "skip", "未指定通道类型"))
        checks.append(_result("image_arch_match", "镜像", "镜像架构匹配宿主", "skip", "未指定通道类型"))
        image_ok = False

    checks.append(check_vpn_network(dc, vpn_net))
    checks.append(check_dev_net_tun(dc, image, image_ok) if image
                  else _result("dev_net_tun", "运行条件", "/dev/net/tun 可用", "skip", "未指定通道类型"))
    checks.append(check_disk_space(dc))
    if scope == "full":
        checks.append(check_docker_version(dc))
        checks.append(_result("host_arch", "引擎", "宿主架构", "pass", host_arch))
        checks.append(check_mirror_reachable(mirrors))
        if mihomo_alive is None:
            import manager
            mihomo_alive = manager.mihomo_alive()
        checks.append(check_mihomo(mihomo_alive))
    return _aggregate(checks, host_arch, image)


def _aggregate(checks, host_arch, image):
    overall = "pass"
    for c in checks:
        if _SEVERITY[c["status"]] > _SEVERITY[overall]:
            overall = c["status"]
    return {"host_arch": host_arch, "target_image": image,
            "overall": overall, "checks": checks}


_TASKS = {}   # task_id -> {status, progress, log_tail, error}


def _mirror_reachable(host, timeout=5):
    try:
        return requests.get(f"https://{host}/v2/", timeout=timeout).status_code < 500
    except Exception:
        return False


def _log(st, line):
    st["log_tail"] = (st["log_tail"] + [line])[-20:]


def _pull_worker(dc, image, host_arch, mirrors, st):
    repo, _, tag = image.partition(":")
    tag = tag or "latest"
    platform = f"linux/{host_arch}"
    for m in mirrors:
        try:
            st["progress"] = f"探测镜像源 {m}…"
            if not _mirror_reachable(m):
                _log(st, f"{m} 不可达,跳过")
                continue
            src = f"{m}/{repo}"
            st["progress"] = f"从 {m} 拉取 {repo}:{tag}({platform})…"
            img = dc.images.pull(src, tag=tag, platform=platform)
            arch = (getattr(img, "attrs", {}) or {}).get("Architecture")
            if arch and arch != host_arch:
                _log(st, f"{m} 拉到 {arch}(非 {host_arch}),弃用")
                dc.images.remove(f"{src}:{tag}", force=True)
                continue
            img.tag(repo, tag)
            dc.images.remove(f"{src}:{tag}", force=True)
            st["progress"] = f"完成:{repo}:{tag}({arch or host_arch})"
            st["status"] = "done"
            return
        except Exception as e:
            _log(st, f"{m} 失败:{type(e).__name__}: {e}")
            continue
    st["status"] = "error"
    st["error"] = "所有镜像源均失败,建议配置 Docker daemon 国内源后重试(见教程)"


def start_pull(dc, image, host_arch, mirrors=None):
    mirrors = mirrors or DEFAULT_MIRRORS
    tid = uuid.uuid4().hex[:8]
    _TASKS[tid] = {"status": "running", "progress": "准备拉取…", "log_tail": [], "error": None}
    threading.Thread(target=_pull_worker,
                     args=(dc, image, host_arch, mirrors, _TASKS[tid]), daemon=True).start()
    return tid


def get_task(tid):
    return _TASKS.get(tid)
