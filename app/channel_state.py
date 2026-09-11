"""通道级操作锁、探活合并和缓存；状态回写必须匹配当前 generation。"""
import concurrent.futures
import contextlib
import functools
import threading
import time
import logging

import store


class Slot:
    def __init__(self):
        self.operation = threading.RLock()
        self.probes = threading.Lock()
        self.generation = 0
        self.pending = None
        self.cached = None
        self.failures = 0


_lock = threading.Lock()
_slots = {}
_pool = concurrent.futures.ThreadPoolExecutor(max_workers=8, thread_name_prefix="vpn-probe")
_closing = False


def startup():
    global _pool, _closing
    with _lock:
        if _closing:
            _slots.clear()
            _pool = concurrent.futures.ThreadPoolExecutor(max_workers=8, thread_name_prefix="vpn-probe")
            _closing = False


def slot(cid):
    with _lock:
        return _slots.setdefault(cid, Slot())


@contextlib.contextmanager
def mutation(cid):
    current = slot(cid)
    with current.operation:
        if _closing:
            raise RuntimeError("服务正在退出")
        current.generation += 1
        yield


def serialized(fn):
    @functools.wraps(fn)
    def wrapped(cid, *args, **kwargs):
        with mutation(cid):
            return fn(cid, *args, **kwargs)
    return wrapped


def accessed(fn):
    @functools.wraps(fn)
    def wrapped(cid, *args, **kwargs):
        with slot(cid).operation:
            if _closing: raise RuntimeError("服务正在退出")
            return fn(cid, *args, **kwargs)
    return wrapped


def _cleanup_replacement(cid):
    import replacement
    with slot(cid).operation:
        if _closing: return
        try: replacement.cleanup(cid)
        except Exception: logging.getLogger(__name__).warning("通道 %s 的旧资源清理待重试", cid)


def _execute(current, generation, ch):
    import manager
    import replacement
    import replacement_store
    result = manager.probe(ch)
    with current.operation:
        if generation != current.generation or _closing:
            return generation, None
        ok, ms = result
        status = "logged_in" if ok else "running"
        store.set_probe_result(ch["id"], status, ms)
        if ok:
            try: replacement.confirmed(ch['id'])
            except Exception: logging.getLogger(__name__).warning("通道 %s 的替换确认待重试", ch['id'])
        pending = replacement_store.public_status(ch['id'])
        if pending and pending['phase'] in ('committed', 'rolled_back'):
            _pool.submit(_cleanup_replacement, ch['id'])
        value = dict(status=status, connected=ok, latency_ms=ms,
                     checked_at=int(time.time() * 1000), stale=False, replacement=pending)
        with current.probes:
            current.failures = 0 if ok else current.failures + 1
            delay = 30 if ok else min(30, 5 * (2 ** min(current.failures - 1, 3)))
            current.cached = (generation, time.monotonic() + delay, value)
        return generation, value


def sample(cid, fresh=True):
    import replacement_store, stop_intents
    current = slot(cid)
    while True:
        with current.operation:
            if _closing:
                raise RuntimeError("服务正在退出")
            ch = store.get_channel(cid)
            if ch is None:
                return None
            pending = replacement_store.public_status(cid)
            if stop_intents.pending(cid):
                return dict(status="stopped", connected=False, latency_ms=None, checked_at=None, stale=False, replacement=pending, stop_pending=True)
            if pending and pending['phase'] not in ('awaiting_login', 'committed', 'rolled_back'):
                return dict(status='error', connected=False, latency_ms=None, checked_at=None, stale=True, replacement=pending)
            generation = current.generation
        if ch["status"] not in ("running", "logged_in"):
            return dict(status=ch["status"], connected=False, latency_ms=None,
                        checked_at=None, stale=False, replacement=pending)
        stale = None
        with current.probes:
            if _closing:
                raise RuntimeError("服务正在退出")
            if generation != current.generation:
                continue
            if not fresh and current.cached:
                old, until, value = current.cached
                if old == generation and time.monotonic() < until:
                    return value.copy()
                if old == generation:
                    stale = dict(value, stale=True)
            if current.pending is None or current.pending.done():
                current.pending = _pool.submit(_execute, current, generation, ch)
            pending = current.pending
        if stale is not None:
            return stale
        old, value = pending.result(timeout=35)
        if old != current.generation or value is None:
            continue
        return value.copy()


def shutdown():
    global _closing
    with _lock:
        _closing = True
        active = list(_slots.values())
    for current in active:
        with current.operation:
            current.generation += 1
    _pool.shutdown(wait=True)
