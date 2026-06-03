"""从 Docker Hub registry API 拉镜像 tag,过滤出真实版本号,按架构标注可用性。

只保留形如 7.6.3 的语义版本 tag(排除 latest/vncless/cli/dev-*/actions-test-*/
cron-test-* 等变体与 CI tag)。带进程内缓存 + 离线兜底。
"""
import re
import time
import requests

_API = "https://hub.docker.com/v2/repositories/{repo}/tags?page_size=100"
_SEMVER = re.compile(r"^\d+\.\d+(?:\.\d+)?$")   # 7.6.3 / 7.6
_CACHE = {}            # repo -> (ts, [version dict])
_TTL = 3600


def _fetch(repo):
    r = requests.get(_API.format(repo=repo), timeout=8)
    r.raise_for_status()
    out = []
    for t in r.json().get("results", []):
        name = t.get("name", "")
        if not _SEMVER.match(name):
            continue
        archs = sorted({i.get("architecture") for i in t.get("images", [])
                        if i.get("architecture")})
        out.append({"tag": name, "arch": archs})
    out.sort(key=lambda v: [int(x) for x in v["tag"].split(".")], reverse=True)
    return out


def versions(repo, host_arch, fallback):
    """返回 [{tag, arch[], usable_here}]。失败回退 fallback(全部标 usable)。"""
    now = time.time()
    cached = _CACHE.get(repo)
    if cached and now - cached[0] < _TTL:
        raw = cached[1]
    else:
        try:
            raw = _fetch(repo)
            _CACHE[repo] = (now, raw)
        except requests.RequestException:
            return [{"tag": t, "arch": [], "usable_here": True} for t in fallback]
    return [{"tag": v["tag"], "arch": v["arch"],
             "usable_here": (not v["arch"]) or (host_arch in v["arch"])}
            for v in raw]
