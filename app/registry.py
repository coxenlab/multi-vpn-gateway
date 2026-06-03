"""适配器注册表:加载 adapters.yaml,提供 get / list / 主机架构判定。"""
import os
import platform
import yaml

_MANIFEST = os.path.join(os.path.dirname(__file__), "adapters.yaml")
_ARCH_MAP = {"x86_64": "amd64", "amd64": "amd64", "aarch64": "arm64", "arm64": "arm64"}


def _load():
    try:
        with open(_MANIFEST, encoding="utf-8") as f:
            data = yaml.safe_load(f)
    except FileNotFoundError as e:
        raise RuntimeError(f"adapters manifest not found: {_MANIFEST}") from e
    return data["adapters"]


_ADAPTERS = _load()


def get(key):
    """返回适配器 spec(浅拷贝)。未知类型抛 KeyError。"""
    if key not in _ADAPTERS:
        raise KeyError(key)
    return dict(_ADAPTERS[key])


def list_adapters():
    """返回 [{key, label, desc, runtime, versioned, arch, login_modes, inputs}]。"""
    out = []
    for key, spec in _ADAPTERS.items():
        out.append({
            "key": key,
            "label": spec.get("label", key),
            "desc": spec.get("desc", ""),
            "runtime": spec.get("runtime"),
            "versioned": bool(spec.get("versioned")),
            "arch": spec.get("arch", []),
            "login_modes": spec.get("login_modes", []),
            "inputs": spec.get("inputs", []),
        })
    return out


def host_arch():
    return _ARCH_MAP.get(platform.machine().lower(), "unknown")
