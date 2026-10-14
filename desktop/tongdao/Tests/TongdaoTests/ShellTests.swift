import XCTest
@testable import Tongdao

final class ShellTests: XCTestCase {
    private func channel(_ id: String, status: String, login: String = "gui", patch: [String: Any] = [:]) throws -> Channel {
        var object: [String: Any] = ["id": id, "name": "示例通道", "vpn_type": "wireguard", "server": "vpn.example.test", "username": "",
            "status": status, "login_method": login, "probe_url": "", "routing_enabled": true, "domains": [], "ips": []]
        object.merge(patch) { _, new in new }
        return try JSONDecoder().decode(Channel.self, from: JSONSerialization.data(withJSONObject: object))
    }

    func testPanelRowTextAndActionFollowTheMainWindow() throws {
        let connected = MenuBarPanel.row(try channel("c1", status: "logged_in", patch: ["latency_ms": 18]))
        XCTAssertEqual(connected.text, "已连接，18 毫秒"); XCTAssertEqual(connected.action, .toggle(on: true))
        XCTAssertEqual(MenuBarPanel.row(try channel("c1", status: "logged_in")).text, "已连接")
        let unknown = try channel("pending", status: "logged_in", patch: ["container_state": "unknown"])
        XCTAssertEqual(MenuBarPanel.row(unknown).text, "状态待确认")
        XCTAssertEqual(MenuBarPanel.row(unknown).action, .view("#/channels/pending"))
        XCTAssertEqual(MenuBarPanel.onlineCount([unknown]), 0)

        let stopped = MenuBarPanel.row(try channel("c2", status: "stopped"))
        XCTAssertEqual(stopped.text, "已停止"); XCTAssertEqual(stopped.action, .toggle(on: false))
        XCTAssertEqual(MenuBarPanel.row(try channel("c2", status: "logged_in", patch: ["stop_pending": true])).action, .toggle(on: false))

        for login in ["gui", "byo"] {
            let waiting = MenuBarPanel.row(try channel("c3", status: "running", login: login))
            XCTAssertEqual(waiting.text, "待登录"); XCTAssertEqual(waiting.action, .login("#/channels/c3?tab=login"))
        }
        for status in ["running", "creating"] {
            let headless = MenuBarPanel.row(try channel("c4", status: status, login: "headless"))
            XCTAssertEqual(headless.text, "待登录"); XCTAssertEqual(headless.action, .view("#/channels/c4"))
        }

        for status in ["error", "down"] {
            let failed = MenuBarPanel.row(try channel("c5", status: status))
            XCTAssertEqual(failed.text, "出错"); XCTAssertEqual(failed.action, .view("#/channels/c5"))
        }
        let replacing = MenuBarPanel.row(try channel("c6", status: "running", patch: ["replacement": ["phase": "starting", "can_restore": true]]))
        XCTAssertEqual(replacing.text, "出错"); XCTAssertEqual(replacing.action, .view("#/channels/c6"))
        XCTAssertEqual(MenuBarPanel.row(try channel("c7", status: "running", patch: ["replacement": ["phase": "awaiting_login", "can_restore": true]])).action,
                       .login("#/channels/c7?tab=login"))
    }

    func testMenuBarCountShowsOnlineChannelsOnceReady() throws {
        let channels = [try channel("a", status: "logged_in"), try channel("b", status: "logged_in"), try channel("c", status: "running"),
                        try channel("d", status: "logged_in", patch: ["stop_pending": true]), try channel("e", status: "error")]
        XCTAssertEqual(MenuBarPanel.menuBarCount(ready: true, channels: channels), "2")
        XCTAssertEqual(MenuBarPanel.menuBarCount(ready: true, channels: []), "0")
        XCTAssertNil(MenuBarPanel.menuBarCount(ready: false, channels: channels))
    }

    func testClosedWindowStaysPausedWhenTheAppIsUnhidden() {
        XCTAssertFalse(AppDelegate.pageVisible(windowVisible: false, occlusionVisible: false, appHidden: false), "closed to the menu bar")
        XCTAssertFalse(AppDelegate.pageVisible(windowVisible: false, occlusionVisible: true, appHidden: false))
        XCTAssertFalse(AppDelegate.pageVisible(windowVisible: true, occlusionVisible: true, appHidden: true))
        XCTAssertFalse(AppDelegate.pageVisible(windowVisible: true, occlusionVisible: false, appHidden: false))
        XCTAssertTrue(AppDelegate.pageVisible(windowVisible: true, occlusionVisible: true, appHidden: false))
    }

    func testPendingJumpRunsOnceAfterTheLoadFinishes() {
        var jumps = PendingJumps()
        XCTAssertNil(jumps.request("#/channels/c1?tab=login"), "nothing is sent before the first load")
        XCTAssertEqual(jumps.finished(), ["#/channels/c1?tab=login"])
        XCTAssertEqual(jumps.finished(), [], "a finished jump is not sent again")
        XCTAssertEqual(jumps.request("#/channels/c2"), "#/channels/c2", "a loaded page takes the jump at once")

        jumps.started()
        XCTAssertNil(jumps.request("#/channels/c3"))
        XCTAssertNil(jumps.request("#/channels/c4"))
        XCTAssertEqual(jumps.finished(), ["#/channels/c3", "#/channels/c4"])

        jumps.undelivered("#/channels/c5")
        XCTAssertFalse(jumps.loading)
        XCTAssertEqual(jumps.request("#/settings"), "#/settings")
        XCTAssertEqual(jumps.finished(), ["#/channels/c5"])
        XCTAssertTrue(jumps.undelivered("#/channels/c5"), "second failure allows one fallback reload")
        XCTAssertFalse(jumps.undelivered("#/channels/c5"), "fallback must not create a reload loop")
        XCTAssertEqual(jumps.finished(), [])
    }

    func testStaleMenuCountAndLocalJumpFallback() throws {
        let list = [try channel("online", status: "logged_in")]
        XCTAssertEqual(MenuBarPanel.menuBarCount(ready: true, channels: list, stale: true), "1·")
        XCTAssertEqual(MenuBarPanel.headerTitle(ready: true, channels: list, stale: true), "状态待确认")
        let origin = URL(string: "http://127.0.0.1:41234/")!
        XCTAssertEqual(WebWorkspaceController.fallbackURL(origin: origin, current: nil, target: "#/settings")?.fragment, "/settings")
        let current = URL(string: "http://127.0.0.1:41234/#/channels/c1?tab=login")!
        XCTAssertEqual(WebWorkspaceController.fallbackURL(origin: origin, current: current, target: "?sheet=add")?.fragment, "/channels/c1?tab=login&sheet=add")
        XCTAssertEqual(WebWorkspaceController.fallbackURL(origin: origin, current: nil, target: "?sheet=add")?.fragment, "/home?sheet=add")
        XCTAssertNil(WebWorkspaceController.fallbackURL(origin: origin, current: current, target: "https://example.invalid/"))
    }

    func testReplacingAnExportRemovesTheOldFileButNeverAFolder() throws {
        let directory = URL(fileURLWithPath: "/tmp").appendingPathComponent("tongdao-native-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let file = directory.appendingPathComponent("通道管家-Clash-Verge.yaml")
        try Data("old".utf8).write(to: file)
        XCTAssertEqual(WebWorkspaceController.replacing(file), file)
        XCTAssertFalse(FileManager.default.fileExists(atPath: file.path))

        let fresh = directory.appendingPathComponent("new.jsonl")
        XCTAssertEqual(WebWorkspaceController.replacing(fresh), fresh)
        let folder = directory.appendingPathComponent("folder")
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        XCTAssertEqual(WebWorkspaceController.replacing(folder), folder)
        XCTAssertTrue(FileManager.default.fileExists(atPath: folder.path))
        XCTAssertNil(WebWorkspaceController.replacing(nil))
    }
}
