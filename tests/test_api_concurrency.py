import asyncio
import threading

import httpx
import pytest


@pytest.mark.parametrize("operation", ["add", "toggle", "batch", "import"])
def test_config_writes_leave_the_event_loop_responsive(client, make_channel, monkeypatch, operation):
    import main, manager, store

    store.add_channel(make_channel("responsive"))
    rid = store.add_rule("responsive", "domain", "old.example")
    requests = {
        "add": ("POST", "/api/channels/responsive/rules", {"patterns": ["new.example"]}),
        "toggle": ("PATCH", f"/api/channels/responsive/rules/{rid}", {"enabled": False}),
        "batch": ("PATCH", "/api/rules", {"ids": [rid], "enabled": False}),
        "import": ("POST", "/api/config/import", {"kind": "vpnmgr-export", "channels": [
            {"name": "imported", "vpn_type": "easyconnect", "rules": []}]}),
    }
    entered, release = threading.Event(), threading.Event()
    timeouts = []

    def slow_rebuild():
        entered.set()
        if not release.wait(2):
            timeouts.append(True)
        return 204

    monkeypatch.setattr(manager, "rebuild", slow_rebuild)

    async def run():
        async with httpx.AsyncClient(transport=httpx.ASGITransport(app=main.app), base_url="http://fixture") as api:
            method, path, body = requests[operation]
            write = asyncio.create_task(api.request(method, path, json=body))
            try:
                assert await asyncio.to_thread(entered.wait, 1)
                read = await asyncio.wait_for(api.get("/api/vpn-types"), 0.5)
                assert read.status_code == 200
                assert not write.done() and not timeouts, "slow config write blocked unrelated API reads"
            finally:
                release.set()
                result = await write
            assert result.status_code == 200
            if operation == "add":
                assert result.json()["added"] == {"domain": 1, "ip": 0}
            elif operation == "import":
                assert result.json()["imported"] == ["imported"]
            else:
                assert store.list_rules("responsive")[0]["enabled"] == 0

    asyncio.run(run())


def test_cancelled_import_finishes_accepted_work_without_replaying_it(client, monkeypatch):
    import main, store

    entered, release, finished = threading.Event(), threading.Event(), threading.Event()
    original = store.import_channels
    calls = []

    def delayed_import(plans):
        calls.append(len(plans))
        if len(calls) == 1:
            entered.set()
            assert release.wait(3)
        try:
            return original(plans)
        finally:
            finished.set()

    monkeypatch.setattr(store, "import_channels", delayed_import)
    document = {"kind": "vpnmgr-export", "channels": [{"name": "retained", "vpn_type": "easyconnect"}]}

    async def run():
        async with httpx.AsyncClient(transport=httpx.ASGITransport(app=main.app), base_url="http://fixture") as api:
            request = asyncio.create_task(api.post("/api/config/import", json=document))
            try:
                assert await asyncio.to_thread(entered.wait, 1)
                request.cancel()
                with pytest.raises(asyncio.CancelledError):
                    await request
            finally:
                release.set()
            assert await asyncio.to_thread(finished.wait, 1)
            # A later explicit retry sees the committed result instead of creating another copy.
            retry = await api.post("/api/config/import", json=document)
            assert retry.status_code == 200 and retry.json()["imported"] == []
            assert retry.json()["skipped"][0]["name"] == "retained"
            assert calls == [1, 0]
            assert [channel["name"] for channel in store.list_channels()] == ["retained"]

    asyncio.run(run())


def test_concurrent_imports_keep_duplicate_detection_and_one_transaction(client, monkeypatch):
    import main, store

    entered, release = threading.Event(), threading.Event()
    original = store.import_channels
    calls = []

    def delayed_import(plans):
        calls.append(len(plans))
        if len(calls) == 1:
            entered.set()
            assert release.wait(3)
        return original(plans)

    monkeypatch.setattr(store, "import_channels", delayed_import)
    document = {"kind": "vpnmgr-export", "channels": [{"name": "one-copy", "vpn_type": "easyconnect",
        "rules": [{"kind": "domain", "pattern": "one.example", "enabled": False}]}]}

    async def run():
        async with httpx.AsyncClient(transport=httpx.ASGITransport(app=main.app), base_url="http://fixture") as api:
            first = asyncio.create_task(api.post("/api/config/import", json=document))
            second = None
            try:
                assert await asyncio.to_thread(entered.wait, 1)
                second = asyncio.create_task(api.post("/api/config/import", json=document))
                assert (await api.get("/api/vpn-types")).status_code == 200
                await asyncio.sleep(0.05)
                assert not first.done() and not second.done()
                assert calls == [1], "second plan must wait until duplicate names can be read back"
            finally:
                release.set()
                results = await asyncio.gather(first, *([second] if second else []))
            assert [result.status_code for result in results] == [200, 200]
            assert results[0].json()["imported"] == ["one-copy"]
            assert results[1].json()["imported"] == []
            assert results[1].json()["skipped"][0]["name"] == "one-copy"
            channels = store.list_channels()
            assert len(channels) == 1 and channels[0]["status"] == "stopped"
            assert len(store.list_rules(channels[0]["id"])) == 1
            assert store.list_rules(channels[0]["id"])[0]["enabled"] == 0

    asyncio.run(run())
