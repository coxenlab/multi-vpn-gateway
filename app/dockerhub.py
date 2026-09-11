"""从 Docker Hub registry API 拉镜像 tag,过滤出真实版本号,按架构标注可用性。

只保留形如 7.6.3 的语义版本 tag(排除 latest/vncless/cli/dev-*/actions-test-*/
cron-test-* 等变体与 CI tag)。带进程内缓存 + 离线兜底。
"""
import re
import time
import requests
from urllib.parse import urljoin, urlsplit

_API = "https://hub.docker.com/v2/namespaces/{namespace}/repositories/{repository}/tags?page_size=100"
_SEMVER = re.compile(r"^\d+\.\d+(?:\.\d+)?$")   # 7.6.3 / 7.6
_CACHE = {}            # repo -> (ts, [version dict])
_TTL = 3600


def _fetch(repo):
    namespace, repository = repo.split("/", 1)
    url = _API.format(namespace=namespace, repository=repository)
    origin = urlsplit(url)
    deadline = time.monotonic() + 8
    seen, versions = set(), {}
    while url:
        page = urlsplit(url)
        if (page.scheme, page.netloc, page.path.rstrip("/")) != (origin.scheme, origin.netloc, origin.path.rstrip("/")):
            raise ValueError("unexpected Docker Hub pagination URL")
        remaining = deadline - time.monotonic()
        if url in seen or len(seen) >= 100 or remaining <= 0:
            raise ValueError("Docker Hub pagination did not complete")
        seen.add(url)
        r = requests.get(url, timeout=remaining, allow_redirects=False)
        r.raise_for_status()
        if r.status_code in (301, 302, 303, 307, 308):
            location = r.headers.get("Location")
            if not location:
                raise ValueError("invalid Docker Hub redirect")
            url = urljoin(url, location)
            continue  # The next iteration validates origin, resource path and cycles.
        if r.status_code != 200:
            raise ValueError("unexpected Docker Hub response")
        body = r.json()
        if not isinstance(body, dict) or not isinstance(body.get("results"), list):
            raise ValueError("invalid Docker Hub tag page")
        for t in body["results"]:
            if not isinstance(t, dict):
                continue
            name = t.get("name", "")
            if not isinstance(name, str) or not _SEMVER.fullmatch(name) or name in versions:
                continue
            images = t.get("images") or []
            if not isinstance(images, list):
                raise ValueError("invalid Docker Hub architecture list")
            archs = sorted({i["architecture"] for i in images if isinstance(i, dict)
                            and isinstance(i.get("architecture"), str) and i["architecture"]})
            versions[name] = {"tag": name, "arch": archs}
        nxt = body.get("next")
        if nxt is not None and not isinstance(nxt, str):
            raise ValueError("invalid Docker Hub next page")
        url = urljoin(url, nxt) if nxt else None
    out = list(versions.values())
    out.sort(key=lambda v: [int(x) for x in v["tag"].split(".")], reverse=True)
    return out


def versions(repo, host_arch, fallback):
    """仅完整分页结果入缓存；失败保留完整旧缓存，无缓存时使用声明的兜底版本。"""
    now = time.time()
    cached = _CACHE.get(repo)
    if cached and now - cached[0] < _TTL:
        raw = cached[1]
    else:
        try:
            raw = _fetch(repo)
            _CACHE[repo] = (now, raw)
        except (requests.RequestException, ValueError):
            if cached:
                raw = cached[1]
            else:
                return [{"tag": t, "arch": [], "usable_here": True} for t in fallback]
    return [{"tag": v["tag"], "arch": v["arch"],
             "usable_here": (not v["arch"]) or (host_arch in v["arch"])}
            for v in raw]
