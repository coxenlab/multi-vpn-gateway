"""托管规则/代理读回与串行应用；运行态确认不代表 DNS/TUN/企业 VPN 健康。"""
from contextlib import contextmanager
import copy
import fcntl
import hashlib
import math
import os
from pathlib import Path
import struct
import time
import requests
import yaml
import config_apply_store as journal
import store

PREFIX = "__vpnmgr_cfg_"


def fingerprint(value):
    # 有类型、长度和排序的编码；浮点用 IEEE bytes，避免两语言 JSON 指数格式差异。
    def encode(v):
        if v is None: return b"n"
        if isinstance(v, bool): return b"t" if v else b"f"
        if isinstance(v, int):
            if not -(2**63) <= v < 2**64: raise ValueError("配置数字超出范围")
            return b"i" + str(v).encode() + b";"
        if isinstance(v, float):
            if not math.isfinite(v): raise ValueError("配置数字无效")
            return b"d" + struct.pack(">d", v)
        if isinstance(v, str):
            raw = v.encode()
            return b"s" + str(len(raw)).encode() + b":" + raw
        if isinstance(v, list): return b"a" + str(len(v)).encode() + b":" + b"".join(map(encode, v))
        if isinstance(v, dict) and all(isinstance(k, str) for k in v):
            return b"o" + str(len(v)).encode() + b":" + b"".join(encode(k) + encode(v[k]) for k in sorted(v))
        raise ValueError("配置类型不支持")
    return hashlib.sha256(b"vpnmgr-config-v1\0" + encode(value)).hexdigest()


def marker(ticket):
    return f"{PREFIX}{ticket.generation}_{ticket.digest}"


def stamp(config, ticket):
    value = copy.deepcopy(config)
    value["proxies"].append({"name": marker(ticket), "type": "direct"})
    return value


def matches(config, ticket, proxies, rules, general):
    if not all(isinstance(v, dict) for v in (proxies, rules, general)): return False
    entries = proxies.get("proxies", {})
    if not isinstance(entries, dict) or any(not isinstance(v, dict) for v in entries.values()): return False
    expected = {p["name"] for p in config["proxies"]}
    actual = {name for name in entries if name.startswith("ch-")}
    marks = {name for name in entries if name.startswith(PREFIX)}
    if actual != expected or marks != {marker(ticket)}: return False
    if entries[marker(ticket)].get("type") != "Direct": return False
    if any(entries[name].get("type") != "Socks5" for name in expected): return False
    if general.get("mode") != config.get("mode", "rule"): return False
    live_rules = rules.get("rules")
    if not isinstance(live_rules, list) or len(live_rules) != len(config["rules"]): return False
    for wanted, actual in zip(config["rules"], live_rules):
        if not isinstance(actual, dict) or not isinstance(actual.get("extra", {}), dict): return False
        parts = wanted.split(",")
        if parts[0] == "MATCH": kind, payload, proxy = "Match", "", parts[1]
        elif parts[0] == "DOMAIN-SUFFIX": kind, payload, proxy = "DomainSuffix", parts[1], parts[2]
        elif parts[0] == "IP-CIDR": kind, payload, proxy = "IPCIDR", parts[1], parts[2]
        else: return False
        if (actual.get("type"), actual.get("payload"), actual.get("proxy")) != (kind, payload, proxy): return False
        if actual.get("extra", {}).get("disabled", False): return False
    return True


@contextmanager
def application_lock():
    path = Path(store.DATA_DIR) / "mihomo-apply.lock"
    fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o600)
    try:
        deadline = time.monotonic() + 45
        while True:
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                if time.monotonic() >= deadline: raise TimeoutError("配置应用仍在处理中")
                time.sleep(.05)
        yield
    finally:
        os.close(fd)


def readback(config, ticket, controller, secret):
    headers = {"Authorization": f"Bearer {secret}"}
    def get(path):
        response = requests.get(f"{controller}/{path}", headers=headers, timeout=3)
        response.raise_for_status()
        return response.json()
    proxies = get("proxies")
    rules = get("rules")
    general = get("configs")
    # 应用先更新 proxies 再更新 rules；跨过另一次切换时拒绝混合的控制器读回。
    return matches(config, ticket, proxies, rules, general) and matches(config, ticket, get("proxies"), rules, general)


def apply(config_path, controller, secret, render, write_yaml):
    with application_lock():
        try:
            with open(config_path) as source:
                base = yaml.safe_load(source) or {}
        except FileNotFoundError:
            base = {}
        previous = journal.status()
        for _ in range(8):
            revision, channels, rules = journal.snapshot()
            config = render(base, channels, rules)
            digest = fingerprint(config)
            try:
                ticket = journal.prepare(revision, False, digest)
                break
            except RuntimeError:
                continue
        else:
            return "配置持续变化，已保存但尚未同步，请重试"
        stamped = stamp(config, ticket)
        try:
            verified = readback(config, ticket, controller, secret)
        except (requests.RequestException, ValueError, TypeError, KeyError):
            verified = False
        needs_flush = verified and (previous["desired_revision"] != ticket.revision or previous["last_error"] is not None)
        reload_failed = False
        if not verified:
            try:
                response = requests.put(f"{controller}/configs", params={"force": "true"},
                    json={"payload": yaml.safe_dump(stamped, allow_unicode=True, sort_keys=False)},
                    headers={"Authorization": f"Bearer {secret}"}, timeout=10)
                reload_failed = not 200 <= response.status_code < 300
            except requests.RequestException:
                reload_failed = True
            # 写响应不明只读回；本次不盲目重发，也不把丢失的 HTTP ACK 当完整成功。
            try:
                verified = readback(config, ticket, controller, secret)
            except (requests.RequestException, ValueError, TypeError, KeyError):
                journal.failed(ticket, "readback_failed")
                return "配置已保存，但无法确认规则应用，请重试"
        if not verified:
            journal.failed(ticket, "reload_failed" if reload_failed else "readback_mismatch")
            return "配置已保存，但运行规则未匹配，请重试"
        journal.observed(ticket)
        if reload_failed:
            journal.failed(ticket, "reload_failed")
            return "规则已读回，但重载响应未确认，请重试"
        if needs_flush:
            try:
                response = requests.post(f"{controller}/cache/dns/flush",
                    headers={"Authorization": f"Bearer {secret}"}, timeout=3)
                if not 200 <= response.status_code < 300:
                    raise requests.HTTPError("DNS 缓存刷新未确认")
            except requests.RequestException:
                journal.failed(ticket, "dns_flush_failed")
                return "规则已读回，但通道地址缓存刷新未确认，请重试"
        # 解析失败不会覆盖上次启动文件；同内容且运行态一致时连写盘也跳过。
        if base != stamped:
            try:
                write_yaml(config_path, stamped)
            except OSError:
                journal.failed(ticket, "write_failed")
                return "运行规则已应用，但启动配置保存失败，请重试"
        if base != stamped or needs_flush:
            try:
                current = readback(config, ticket, controller, secret)
            except (requests.RequestException, ValueError, TypeError, KeyError):
                journal.failed(ticket, "readback_failed")
                return "启动配置已保存，运行规则仍待重新确认，请重试"
            if not current:
                journal.failed(ticket, "readback_mismatch")
                return "启动配置已保存，但内核配置已变化，请重试"
        journal.confirmed(ticket)
        if journal.status()["pending"]:
            return "已有更新配置保存，尚未全部同步，请重试"
        return 204
