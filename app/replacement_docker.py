"""替换专用候选与卷复制。复制源只读，参数不包含 VPN 凭据。"""
from dataclasses import dataclass
import re
import docker.errors
from requests.exceptions import RequestException

CHANNEL = "io.vpnmgr.channel"
OPERATION = "io.vpnmgr.replacement"
ROLE = "io.vpnmgr.role"
COPY_SCRIPT = "set -euo pipefail; find /target -mindepth 1 -delete; tar --numeric-owner --xattrs --acls -C /source -cpf - . | tar --numeric-owner --xattrs --acls -C /target -xpf -"


@dataclass
class Owner:
    channel: str
    operation: str

    def validate(self):
        if not all(re.fullmatch(r"[A-Za-z0-9_-]{1,64}", s) for s in (self.channel, self.operation)):
            raise ValueError("无效的替换资源标识")

    @property
    def candidate_name(self): return f"vpn-{self.channel}-next-{self.operation}"
    @property
    def volume_name(self): return f"vpndata-{self.channel}-next-{self.operation}"
    @property
    def copy_name(self): return f"vpn-copy-{self.channel}-{self.operation}"

    def labels(self, role): return {CHANNEL: self.channel, OPERATION: self.operation, ROLE: role}
    def owns(self, labels, role): return all((labels or {}).get(k) == v for k, v in self.labels(role).items())


def _volume_valid(name):
    return len(name) <= 180 and re.fullmatch(r"vpndata-[A-Za-z0-9_.-]+", name)


def use_volume(kwargs, volume):
    if not _volume_valid(volume): raise ValueError("无效的通道数据卷名")
    volumes = kwargs.get("volumes", {})
    if len(volumes) != 1: raise ValueError("通道数据卷布局不受支持")
    mount = next(iter(volumes.values()))
    if mount.get("bind") not in ("/root", "/config"): raise ValueError("通道数据卷目标不受支持")
    kwargs["volumes"] = {volume: dict(mount)}


def ensure_volume(dc, owner):
    owner.validate()
    try:
        volume = dc.volumes.get(owner.volume_name)
    except docker.errors.NotFound:
        try:
            dc.volumes.create(name=owner.volume_name, driver="local", labels=owner.labels("candidate"))
        except RequestException:
            pass  # 响应不明先读回，不能换名再造。
        volume = dc.volumes.get(owner.volume_name)
    if not owner.owns(volume.attrs.get("Labels"), "candidate"):
        raise RuntimeError("候选数据卷归属不匹配")
    return owner.volume_name


def create_candidate(dc, kwargs, owner):
    owner.validate()
    if kwargs.get("name") != owner.candidate_name: raise ValueError("候选容器名称与操作不匹配")
    try:
        container = dc.containers.get(owner.candidate_name)
    except docker.errors.NotFound:
        config = dict(kwargs)
        config["labels"] = {**config.get("labels", {}), **owner.labels("candidate")}
        try:
            dc.containers.create(**config)
        except RequestException:
            pass
        container = dc.containers.get(owner.candidate_name)
    container.reload()
    if not owner.owns(container.attrs.get("Config", {}).get("Labels"), "candidate"):
        raise RuntimeError("候选容器归属不匹配")
    if container.attrs.get("State", {}).get("Status") != "created":
        raise RuntimeError("候选容器已启动，需先核对进度")
    return container.id


def copy_volume(dc, owner, old_container, source, image):
    if not old_container: raise ValueError('复制旧通道必须指定原容器 ID')
    _copy_volume(dc, owner, old_container, source, image)


def copy_unattached_volume(dc, owner, source, image):
    """原容器已丢失时只复制无人使用的旧命名卷；不创建缺失的源卷。"""
    _copy_volume(dc, owner, None, source, image)


def _copy_volume(dc, owner, old_container, source, image):
    owner.validate()
    target = owner.volume_name
    if not _volume_valid(source) or source == target: raise ValueError("源卷与候选卷无效或相同")
    if not re.fullmatch(r"sha256:[0-9a-f]{64}", image): raise ValueError("复制工具必须固定到已加载镜像 ID")
    dc.volumes.get(source)  # Docker 的 bind 会自动新建卷，必须先确认源确实存在。
    old = dc.containers.get(old_container) if old_container else None
    if old is not None:
        old.reload()
        if old.id != old_container or old.attrs.get("Name") != f"/vpn-{owner.channel}":
            raise RuntimeError("旧容器身份与通道不匹配")
        if old.attrs.get("State", {}).get("Running") is not False: raise RuntimeError("复制数据前必须停止旧通道")
        if not any(m.get("Name") == source for m in old.attrs.get("Mounts", [])):
            raise RuntimeError("源卷不属于旧容器")
    if not owner.owns(dc.volumes.get(target).attrs.get("Labels"), "candidate"):
        raise RuntimeError("候选数据卷归属不匹配")
    def check_users():
        for name in (source, target):
            for container in dc.containers.list(all=True, filters={"volume": name}):
                container.reload()
                if name == source and old is None: raise RuntimeError("源卷已被其他容器使用")
                if container.attrs.get("State", {}).get("Running") is not False:
                    raise RuntimeError("数据卷仍有运行中的使用者")
                if container.id != old_container and not owner.owns(container.attrs.get("Config", {}).get("Labels"), "candidate"):
                    raise RuntimeError("数据卷被其他容器使用")
    check_users()
    try:
        dc.containers.get(owner.copy_name)
    except docker.errors.NotFound:
        pass
    else:
        raise RuntimeError("存在待核对的复制容器")
    try:
        dc.containers.create(image=image, name=owner.copy_name,
            entrypoint=["bash", "-c", COPY_SCRIPT], labels=owner.labels("copy"),
            network_mode="none", read_only=True,
            volumes={source: {"bind": "/source", "mode": "ro"}, target: {"bind": "/target", "mode": "rw"}},
            cap_drop=["ALL"], cap_add=["CHOWN", "DAC_OVERRIDE", "FOWNER", "FSETID", "SETFCAP", "MKNOD"],
            security_opt=["no-new-privileges:true"])
    except RequestException:
        pass
    copy = dc.containers.get(owner.copy_name); copy.reload()
    if not owner.owns(copy.attrs.get("Config", {}).get("Labels"), "copy"):
        raise RuntimeError("复制容器归属不匹配")
    try:
        try: copy.start()
        except RequestException: pass
        try: copy.wait(timeout=180)
        except Exception: pass
        copy.reload()
        state = copy.attrs.get("State", {})
        if state.get("Status") != "exited" or state.get("ExitCode") != 0:
            raise RuntimeError("数据卷复制未完成")
    finally:
        try: copy.remove(force=True)
        except RequestException: pass
        try: dc.containers.get(copy.id)
        except docker.errors.NotFound: pass
        else: raise RuntimeError("复制容器清理未确认")
    check_users()
    if old is not None:
        old.reload()
        if old.attrs.get("State", {}).get("Running") is not False:
            raise RuntimeError("复制期间旧通道被重新启动，候选数据不可提交")
