from concurrent.futures import ThreadPoolExecutor
import threading
import time

import pytest

import channel_state
import store


def test_probe_requests_share_work(client, make_channel, monkeypatch):
    import manager
    store.add_channel(make_channel("c1"))
    started, release = threading.Event(), threading.Event()
    calls = []

    def probe(ch):
        calls.append(ch["id"])
        started.set()
        assert release.wait(3)
        return True, 12

    monkeypatch.setattr(manager, "probe", probe)
    with ThreadPoolExecutor(max_workers=8) as pool:
        tasks = [pool.submit(client.get, "/api/channels/c1/status") for _ in range(8)]
        assert started.wait(2)
        time.sleep(0.1)
        release.set()
        results = [task.result(3).json() for task in tasks]
    assert calls == ["c1"]
    assert len({result["checked_at"] for result in results}) == 1
    assert all(result["connected"] for result in results)


def test_stop_during_probe_discards_late_success(client, make_channel, monkeypatch):
    import manager
    store.add_channel(make_channel("c1"))
    started, release = threading.Event(), threading.Event()

    def probe(ch):
        started.set()
        assert release.wait(3)
        return True, 12

    monkeypatch.setattr(manager, "probe", probe)
    with ThreadPoolExecutor(max_workers=1) as pool:
        task = pool.submit(client.get, "/api/channels/c1/status")
        assert started.wait(2)
        try:
            assert client.post("/api/channels/c1/stop").status_code == 200
        finally:
            release.set()
        assert task.result(3).json()["status"] == "stopped"
    assert store.get_channel("c1")["status"] == "stopped"


def test_health_cache_manual_refresh_and_failed_latency(client, make_channel, monkeypatch):
    import manager
    store.add_channel(make_channel("c1"))
    answers = iter([(True, 12), (False, None)])
    monkeypatch.setattr(manager, "probe", lambda ch: next(answers))
    first = client.get("/api/channels/c1/health").json()
    assert client.get("/api/channels/c1/health").json() == first
    fresh = client.get("/api/channels/c1/status").json()
    assert fresh["connected"] is False
    assert fresh["latency_ms"] is None
    assert store.get_channel("c1")["latency_ms"] is None


def test_shutdown_waits_for_mutation_and_rejects_new_work():
    entered, release = threading.Event(), threading.Event()

    def change():
        with channel_state.mutation("c1"):
            entered.set()
            assert release.wait(3)

    with ThreadPoolExecutor(max_workers=2) as pool:
        change_task = pool.submit(change)
        assert entered.wait(2)
        stop_task = pool.submit(channel_state.shutdown)
        try:
            deadline = time.monotonic() + 2
            while not channel_state._closing and time.monotonic() < deadline:
                time.sleep(0.005)
            assert not stop_task.done()
            with pytest.raises(RuntimeError, match="退出"):
                with channel_state.mutation("c2"):
                    pytest.fail("new operation admitted while closing")
        finally:
            release.set()
        change_task.result(3)
        stop_task.result(3)


def test_expired_health_returns_unconfirmed_while_refreshing(client, make_channel, monkeypatch):
    import manager
    store.add_channel(make_channel("c1"))
    first = client.get("/api/channels/c1/health").json()
    current = channel_state.slot("c1")
    generation, _, value = current.cached
    current.cached = generation, 0, value
    started, release = threading.Event(), threading.Event()

    def probe(ch):
        started.set()
        assert release.wait(3)
        return False, None

    monkeypatch.setattr(manager, "probe", probe)
    try:
        stale = client.get("/api/channels/c1/health").json()
        assert stale["stale"] is True
        assert stale["checked_at"] == first["checked_at"]
        assert started.wait(2)
    finally:
        release.set()
    current.pending.result(3)
    refreshed = client.get("/api/channels/c1/health").json()
    assert refreshed["stale"] is False and refreshed["connected"] is False
