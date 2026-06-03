import pytest
import registry


def test_lists_two_adapters():
    keys = {a["key"] for a in registry.list_adapters()}
    assert {"easyconnect", "atrust"} <= keys


def test_get_easyconnect_shape():
    spec = registry.get("easyconnect")
    assert spec["runtime"] == "hagb"
    assert spec["versioned"] is True
    assert spec["version_repo"] == "hagb/docker-easyconnect"
    assert spec["image"] == "hagb/docker-easyconnect:{version}"
    assert spec["env"]["EC_VER"] == "{version}"
    assert [i["key"] for i in spec["inputs"]] == ["server", "username", "password"]


def test_get_atrust_not_versioned():
    spec = registry.get("atrust")
    assert spec["versioned"] is False
    assert spec["image"] == "hagb/docker-atrust:latest"
    assert spec["sysctls"] == {"net.ipv4.conf.default.route_localnet": "1"}


def test_unknown_type_raises():
    with pytest.raises(KeyError):
        registry.get("nope")


def test_host_arch_known():
    assert registry.host_arch() in ("amd64", "arm64", "unknown")
