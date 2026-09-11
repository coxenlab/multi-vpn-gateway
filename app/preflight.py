"""Docker 环境/容器体检引擎 + 镜像类自动修复。

检查函数一律接收 docker 客户端作为参数(依赖注入,便于单测),且永不抛异常:
内部任何错误都转成 warn/fail 的 CheckResult。镜像源 P1 硬编码(P2 切到 mirrors 表)。
"""
import threading
import uuid
import time
import requests
import docker
import registry
import dockerhub
import json
from pathlib import Path

MIHOMO_SOURCE = json.loads(Path(__file__).with_name("mihomo-source.json").read_text())
MIHOMO_IMAGE = MIHOMO_SOURCE["image"]

# P1 硬编码镜像源(按顺序探测可达再拉);P2 改为读 store.mirrors 表
# 内置国内 Docker Hub 加速源(优先级序)。⚠️ 收录标准:实测能取到真实 manifest
# (如 metacubex/mihomo),不是 /v2/ 探针 <500 就算——xuanyuan/dockerproxy.net/aityp
# 经红队实测是僵尸源(manifest 403/404/非 registry),已剔除。全死时用户在「镜像源」屏补源。
DEFAULT_MIRRORS = [
    "docker.1ms.run",
    "docker.m.daocloud.io",
    "hub.rat.dev",
]

# 自建镜像的本地构建上下文(镜像名前缀 → 仓库内构建目录)
_BUILD_CONTEXT = {"vpnmgr/oss-vpn": "images/oss", "vpnmgr/byo-desktop": "images/byo",
                  "vpnmgr/hillstone-desktop": "images/hillstone"}

# 基础设施镜像(定义在 docker-compose,不在 adapters):分流底座 + 管理后端
INFRA_IMAGES = [
    {"image": MIHOMO_IMAGE, "kind": "pull", "title": "mihomo 分流底座",
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
        full_spec = registry.get(spec["key"])
        full = full_spec["image"]
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
                "_fallback": full_spec.get("fallback_versions", []),
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


class _TunChecks:
    def __init__(self, clock=time.monotonic):
        self.clock = clock
        self.lock = threading.Lock()
        self.samples = {}

    def sample(self, key, fresh, probe):
        requested = self.clock()
        with self.lock:
            now = self.clock()
            self.samples = {k: (at, check) for k, (at, check) in self.samples.items()
                            if now - at < (60 if check["status"] == "pass" else 5)}
            cached = self.samples.get(key)
            if cached and (not fresh or cached[0] >= requested):
                at, check = cached
                detail = check["detail"]
                return {**check, "detail": (detail + "；" if detail else "")
                        + f"复用 {int(now - at)} 秒前的检测结果"}
            check = probe()
            if len(self.samples) >= 32:
                oldest = min(self.samples, key=lambda k: self.samples[k][0])
                del self.samples[oldest]
            self.samples[key] = (self.clock(), dict(check))
            return check


_tun_checks = _TunChecks()


def _run_tun_probe(dc, image_id):
    token = uuid.uuid4().hex
    name = f"vpncore-tun-probe-{token}"
    container, failure, passed = None, None, False
    try:
        # create 不会像 run 那样在镜像消失后隐式拉取；固定到已检查的 ID。
        container = dc.containers.create(
            image_id, name=name,
            labels={"com.vpnmgr.role": "tun-probe", "com.vpnmgr.operation": token},
            entrypoint=["/bin/sh", "-c", "test -c /dev/net/tun"],
            devices=["/dev/net/tun:/dev/net/tun:rwm"], network_mode="none",
        )
        if not container.id:
            container = None
            raise RuntimeError("探针创建结果缺少 ID")
        container.start()
        status = container.wait(timeout=10)
        if not isinstance(status, dict) or type(status.get("StatusCode")) is not int:
            raise RuntimeError("探针未返回有效退出状态")
        error = status.get("Error")
        if error and (not isinstance(error, dict) or error.get("Message")):
            raise RuntimeError("探针等待接口返回错误")
        passed = status["StatusCode"] == 0
    except Exception as error:
        failure = error
    finally:
        try:
            # 响应丢失只读回本次唯一名称及 labels，不重放创建或删除外来容器。
            if container is None:
                try:
                    candidate = dc.containers.get(name)
                except docker.errors.NotFound:
                    candidate = None
                if candidate is not None:
                    labels = (candidate.attrs.get("Config") or {}).get("Labels") or {}
                    if (labels.get("com.vpnmgr.role") != "tun-probe"
                            or labels.get("com.vpnmgr.operation") != token or not candidate.id):
                        raise RuntimeError("探针归属未确认，未清理同名容器")
                    container = candidate
            if container is not None:
                try:
                    container.remove(force=True, v=True)
                except docker.errors.NotFound:
                    pass
        except Exception as error:
            raise RuntimeError(f"{failure or '检测已结束'}；探针清理未确认: {error}") from error
    if failure is not None:
        raise failure
    return passed


def check_dev_net_tun(dc, image, image_ok, fresh=False):
    """按实际引擎/镜像共享短期检测；完整手动检查重测。warn 级、非阻断。"""
    if not image_ok:
        return _result("dev_net_tun", "运行条件", "/dev/net/tun 可用", "skip",
                       "镜像就绪后检测")
    try:
        engine_id = dc.info().get("ID")
        image_id = dc.images.get(image).id
        if not engine_id or not image_id:
            raise RuntimeError("检测环境缺少引擎或镜像 ID")
        def probe():
            try:
                passed = _run_tun_probe(dc, image_id)
                return _result("dev_net_tun", "运行条件", "/dev/net/tun 可用",
                               "pass" if passed else "warn",
                               "" if passed else "探针未通过，VPN 隧道可能起不来")
            except Exception as e:
                return _result("dev_net_tun", "运行条件", "/dev/net/tun 可用", "warn",
                               f"无法判定:{type(e).__name__}: {e}")
        return _tun_checks.sample((engine_id, image_id), fresh, probe)
    except Exception as e:
        return _result("dev_net_tun", "运行条件", "/dev/net/tun 可用", "warn",
                       f"无法判定:{type(e).__name__}: {e}")


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
    checks.append(check_dev_net_tun(dc, image, image_ok, fresh=scope == "full") if image
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


class PullBusyError(Exception):
    pass


class _PullTasks:
    """Active workers never expire; only completed history is evicted."""
    def __init__(self, clock=time.monotonic):
        self.entries = {}
        self.lock = threading.Lock()
        self.clock = clock

    def _prune(self):
        now = self.clock()
        finished = sorted((e["finished"], tid) for tid, e in self.entries.items()
                          if e["finished"] is not None)
        for index, (when, tid) in enumerate(finished):
            if now - when >= 3600 or index < len(finished) - 32:
                del self.entries[tid]

    def reserve(self, image, arch):
        repo, _, tag = image.partition(":")
        key = (repo + ":" + (tag or "latest"), arch)
        with self.lock:
            self._prune()
            active = [(tid, e) for tid, e in self.entries.items() if e["finished"] is None]
            for tid, entry in active:
                if entry["key"] == key:
                    return tid, False
            if len(active) >= 2:
                raise PullBusyError("已有 2 个镜像正在下载，请等待其中一个完成后重试")
            tid = uuid.uuid4().hex
            self.entries[tid] = {"key": key, "finished": None, "state": {
                "status": "running", "progress": "准备拉取…", "log_tail": [], "error": None}}
            return tid, True

    def publish(self, tid, state, finished=False):
        with self.lock:
            entry = self.entries[tid]
            entry["state"] = {**state, "log_tail": list(state["log_tail"])}
            if finished:
                entry["finished"] = self.clock()
                self._prune()

    def get(self, tid):
        with self.lock:
            self._prune()
            entry = self.entries.get(tid)
            return {**entry["state"], "log_tail": list(entry["state"]["log_tail"])} if entry else None


_TASKS = _PullTasks()


def _mirror_reachable(host, timeout=5):
    try:
        return requests.get(f"https://{host}/v2/", timeout=timeout).status_code < 500
    except Exception:
        return False


def _log(st, line):
    st["log_tail"] = (st["log_tail"] + [line])[-20:]


def _pull_worker(dc, image, host_arch, mirrors, st, on_progress=lambda state: None):
    repo, _, tag = image.partition(":")
    tag = tag or "latest"
    platform = f"linux/{host_arch}"
    pinned = None
    if image == MIHOMO_IMAGE:
        pinned = MIHOMO_SOURCE["platforms"].get(host_arch)
        if pinned is None:
            st.update(status="error", error="mihomo 不支持当前架构")
            return
    for m in mirrors:
        try:
            st["progress"] = f"探测镜像源 {m}…"
            on_progress(st)
            if not _mirror_reachable(m):
                _log(st, f"{m} 不可达,跳过")
                continue
            src = f"{m}/{repo}"
            st["progress"] = f"从 {m} 拉取 {repo}:{tag}({platform})…"
            on_progress(st)
            if pinned:
                img = dc.images.pull(f"{src}@{pinned['manifest']}", platform=platform)
                if getattr(img, "id", None) not in (pinned["config"], pinned["manifest"], MIHOMO_SOURCE["index"]):
                    raise ValueError("mihomo 镜像内容与锁定版本不同，未启用该镜像")
            else:
                img = dc.images.pull(src, tag=tag, platform=platform)
            arch = (getattr(img, "attrs", {}) or {}).get("Architecture")
            if pinned and arch != host_arch:
                raise ValueError("mihomo 镜像架构与锁定来源不同")
            if arch and arch != host_arch:
                _log(st, f"{m} 拉到 {arch}(非 {host_arch}),弃用")
                dc.images.remove(f"{src}:{tag}", force=True)
                continue
            img.tag(repo, tag)
            if not pinned:
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
    tasks = _TASKS
    tid, created = tasks.reserve(image, host_arch)
    if not created:
        return tid
    state = tasks.get(tid)

    def run():
        try:
            _pull_worker(dc, image, host_arch, mirrors or DEFAULT_MIRRORS, state,
                         lambda st: tasks.publish(tid, st))
        except Exception:
            state.update(status="error", error="下载任务意外结束，请核对镜像清单后重试")
        finally:
            # Release capacity only after the actual worker exits, never on a UI timeout.
            if state["status"] == "running":
                state.update(status="error", error="下载任务意外结束，请核对镜像清单后重试")
            tasks.publish(tid, state, finished=True)

    try:
        threading.Thread(target=run, daemon=True).start()
    except RuntimeError:
        state.update(status="error", error="暂时无法启动下载任务，请稍后重试")
        tasks.publish(tid, state, finished=True)
    return tid


def get_task(tid):
    return _TASKS.get(tid)
