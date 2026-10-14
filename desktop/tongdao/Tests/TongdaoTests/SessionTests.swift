import XCTest
import Combine
import Darwin
@testable import Tongdao

/// Deliberately finishes requests after cancellation to exercise late delivery, not just cancellation.
actor DelayedHTTP {
    private struct Pending {
        let request: URLRequest
        let continuation: CheckedContinuation<(Data, URLResponse), Error>
    }
    private var pending: [String: [Pending]] = [:]
    private var arrivals: [String: [XCTestExpectation]] = [:]
    private(set) var count = 0
    func send(_ request: URLRequest) async throws -> (Data, URLResponse) {
        let path = request.url!.path
        count += 1
        return try await withCheckedThrowingContinuation { continuation in
            pending[path, default: []].append(Pending(request: request, continuation: continuation))
            arrivals.removeValue(forKey: path)?.forEach { $0.fulfill() }
        }
    }
    func arrival(_ path: String) -> XCTestExpectation {
        let event = XCTestExpectation(description: "Request arrived: \(path)")
        if pending[path]?.isEmpty == false { event.fulfill() }
        else { arrivals[path, default: []].append(event) }
        return event
    }
    func respond(_ path: String, json: String, status: Int = 200) throws {
        guard var list = pending[path], !list.isEmpty else { throw APIError(message: "Test request did not arrive: \(path)") }
        let item = list.removeFirst(); pending[path] = list
        item.continuation.resume(returning: (Data(json.utf8), HTTPURLResponse(url: item.request.url!, statusCode: status, httpVersion: nil, headerFields: ["Content-Type":"application/json"])!))
    }
    func finish() {
        for request in pending.values.flatMap({ $0 }) { request.continuation.resume(throwing: CancellationError()) }
        pending = [:]
    }
}

final class SessionTests: XCTestCase {
    private actor PollHTTP {
        private(set) var snapshots = 0
        func send(_ request: URLRequest) -> (Data, URLResponse) {
            let json: String
            switch request.url!.path {
            case "/api/channels":
                snapshots += 1
                let status = snapshots == 1 ? "logged_in" : "running"
                json = """
                [{"id":"poll","name":"poll","vpn_type":"easyconnect","server":"fixture.example","username":"","status":"\(status)","login_method":"interactive","probe_url":"","routing_enabled":true,"domains":[],"ips":[]}]
                """
            case "/api/system": json = #"{"runtime":{"phase":"ready"},"mihomo_status":"running"}"#
            default: json = "[]"
            }
            return (Data(json.utf8), HTTPURLResponse(url: request.url!, statusCode: 200, httpVersion: nil, headerFields: nil)!)
        }
    }

    @MainActor func testPeriodicRefreshRunsWhileHiddenAndStopsOnDisconnect() async throws {
        let server = PollHTTP()
        let model = AppModel(refreshInterval: .milliseconds(20))
        model.visible = false
        model.connect(to: LocalAPI(port: 40000, transport: { await server.send($0) }))
        defer { model.disconnect() }
        let deadline = Date().addingTimeInterval(2)
        while await server.snapshots < 2 && Date() < deadline { try await Task.sleep(for: .milliseconds(10)) }
        await model.refresh()
        XCTAssertEqual(MenuBarPanel.onlineCount(model.channels), 0, "hidden menu count follows the latest snapshot")
        let beforeDisconnect = await server.snapshots
        XCTAssertGreaterThanOrEqual(beforeDisconnect, 2)
        model.disconnect()
        try await Task.sleep(for: .milliseconds(40))
        let stopped = await server.snapshots
        try await Task.sleep(for: .milliseconds(100))
        let stillStopped = await server.snapshots
        XCTAssertEqual(stillStopped, stopped)
        model.connect(to: LocalAPI(port: 40000, transport: { await server.send($0) }))
        let reconnectDeadline = Date().addingTimeInterval(2)
        while await server.snapshots == stopped && Date() < reconnectDeadline { try await Task.sleep(for: .milliseconds(10)) }
        let resumed = await server.snapshots
        XCTAssertGreaterThan(resumed, stopped)
    }

    @MainActor private func waitForReady(_ model: AppModel, _ value: Bool) async throws {
        let event = expectation(description: value ? "Owned core ready" : "Owned core exited")
        let observation = model.$ready.filter { $0 == value }.prefix(1).sink { _ in event.fulfill() }
        await fulfillment(of: [event], timeout: 12)
        observation.cancel()
        guard model.ready == value else { throw APIError(message: "Owned core did not reach expected state") }
    }

    @MainActor func testDebugLaunchRequiresIsolatedTongdaoProfile() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("tongdao-native-launch-" + UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let base = ["VPNMGR_DEV_MODE": "1", "VPNMGR_CORE_PATH": root.appendingPathComponent("missing-core").path, "DATA_DIR": root.appendingPathComponent("data").path]
        for profile in ["tongdao", "vpnmgr", "default", "vpnmgr-native-dev", "tongdaodev", ""] {
            var env = base; env["VPNMGR_VM_PROFILE"] = profile
            let model = AppModel(launchEnvironment: env)
            model.launch()
            XCTAssertEqual(model.error, "请通过 desktop/tongdao/dev.sh 启动隔离开发版。", profile)
            XCTAssertFalse(model.starting); XCTAssertFalse(model.ready)
        }
        for missing in ["VPNMGR_DEV_MODE", "VPNMGR_CORE_PATH", "DATA_DIR"] {
            var env = base; env["VPNMGR_VM_PROFILE"] = "tongdao-dev"; env.removeValue(forKey: missing)
            let model = AppModel(launchEnvironment: env)
            model.launch()
            XCTAssertEqual(model.error, "请通过 desktop/tongdao/dev.sh 启动隔离开发版。", missing)
        }
        // An accepted development profile proceeds to launching the configured core; the path
        // does not exist, so the launch fails without spawning anything.
        var env = base; env["VPNMGR_VM_PROFILE"] = "tongdao-dev"
        let model = AppModel(launchEnvironment: env)
        model.launch()
        XCTAssertNotNil(model.error); XCTAssertNotEqual(model.error, "请通过 desktop/tongdao/dev.sh 启动隔离开发版。")
        XCTAssertFalse(model.starting); XCTAssertFalse(model.ready)
        XCTAssertFalse(FileManager.default.fileExists(atPath: root.path), "Rejected launches must not create data")
    }

    @MainActor func testOwnedCoreExitReconnectPreservesDataWithoutStartingVM() async throws {
        guard let path = ProcessInfo.processInfo.environment["TONGDAO_NATIVE_LIFECYCLE_DIR"] else { throw XCTSkip("真实 core 生命周期验证需要专用进程与 Colima 替身目录") }
        guard path.hasPrefix("/tmp/tongdao-native-session-") else { throw APIError(message: "生命周期测试目录必须独立") }
        let root = URL(fileURLWithPath: path)
        let model = AppModel()
        let readPID = { () throws -> pid_t in
            let text = try String(contentsOf: root.appendingPathComponent("data/owned-core.pid"), encoding: .utf8)
            return try XCTUnwrap(pid_t(text.trimmingCharacters(in: .whitespacesAndNewlines)))
        }
        var ownedPID: pid_t?
        defer { if model.ready, let ownedPID { _ = kill(ownedPID, SIGTERM) } }
        model.launch(); try await waitForReady(model, true)
        ownedPID = try readPID()
        let initialAPI = try XCTUnwrap(model.api)
        let config = ["public_config": String(repeating: "x", count: 3 * 1024 * 1024)]
        let backup = try JSONSerialization.data(withJSONObject: ["kind":"vpnmgr-export", "version":1, "channels":[[
            "name":"native-session-fixture", "vpn_type":"easyconnect", "login_method":"interactive", "ec_ver":"7.6.3", "server":"fixture.invalid", "username":"fixture", "probe_url":"https://probe.invalid", "config":config,
            "routing_enabled":false, "rules":[["kind":"domain","pattern":"backup.example","enabled":false,"note":"restore note","locked":true]]
        ] as [String: Any]]])
        let report = try JSONSerialization.jsonObject(with: await initialAPI.data("/api/config/import", method: "POST", rawBody: backup, long: true)) as? [String: Any]
        XCTAssertEqual(report?["imported"] as? [String], ["native-session-fixture"]); XCTAssertEqual(report?["deferred"] as? Bool, true)
        let exported = try JSONSerialization.jsonObject(with: await initialAPI.data("/api/config/export")) as? [String: Any]
        let restored = try XCTUnwrap((exported?["channels"] as? [[String: Any]])?.first)
        XCTAssertEqual(restored["config"] as? [String: String], config)
        XCTAssertEqual(restored["routing_enabled"] as? Bool, false)
        XCTAssertEqual((restored["rules"] as? [[String: Any]])?.first?["note"] as? String, "restore note")
        await model.refresh()
        let channelID = try XCTUnwrap(model.channels.first?.id)
        XCTAssertEqual(model.system?.runtime?.phase, "dormant")
        XCTAssertEqual(kill(try XCTUnwrap(ownedPID), SIGTERM), 0)
        try await waitForReady(model, false)
        XCTAssertNil(model.api); XCTAssertTrue(model.channels.isEmpty); XCTAssertNil(model.system)
        XCTAssertFalse(model.isCurrent(initialAPI))

        model.launch(); try await waitForReady(model, true)
        ownedPID = try readPID()
        // Launch also starts the first refresh; wait for its data without issuing duplicate requests.
        let refreshed = expectation(description: "Persisted channel reloaded")
        let observation = model.$channels.filter { $0.contains(where: { $0.id == channelID }) }.prefix(1).sink { _ in refreshed.fulfill() }
        await fulfillment(of: [refreshed], timeout: 8); observation.cancel()
        XCTAssertEqual(model.channels.map(\.id), [channelID])
        XCTAssertEqual(model.system?.runtime?.phase, "dormant")
        XCTAssertFalse(model.api === initialAPI)
        XCTAssertNil(model.error)
        XCTAssertEqual(kill(try XCTUnwrap(ownedPID), SIGTERM), 0)
        try await waitForReady(model, false)
        let commands = try String(contentsOf: root.appendingPathComponent("colima-commands"), encoding: .utf8).split(separator: "\n").map(String.init)
        XCTAssertEqual(commands, ["stop tongdao-native-session-qa", "stop tongdao-native-session-qa"])
    }

    private func api(_ server: DelayedHTTP) -> LocalAPI { LocalAPI(port: 40000, transport: { try await server.send($0) }) }
    private func wait(_ server: DelayedHTTP, _ path: String) async { await fulfillment(of: [server.arrival(path)], timeout: 2) }
    private func channels(_ id: String) -> String {
        """
        [{"id":"\(id)","name":"\(id)","vpn_type":"easyconnect","server":"fixture.example","username":"fixture","status":"logged_in","configured_status":"running","login_method":"interactive","probe_url":"","routing_enabled":true,"domains":[],"ips":[]}]
        """
    }
    private func completeRefresh(_ server: DelayedHTTP, channel: String,
        system: String = #"{"runtime":{"phase":"ready"},"mihomo_status":"running"}"#,
        channelStatus: Int = 200, systemStatus: Int = 200) async throws {
        await wait(server, "/api/channels"); await wait(server, "/api/system")
        try await server.respond("/api/channels", json: channels(channel), status: channelStatus)
        try await server.respond("/api/system", json: system, status: systemStatus)
        if channelStatus == 200 {
            await wait(server, "/api/vpn-types")
            try await server.respond("/api/vpn-types", json: "[]")
        }
    }

    @MainActor func testSystemFailureDoesNotPreserveHealthyStateOrDiscardValidChannelRefresh() async throws {
        let server = DelayedHTTP(), model = AppModel()
        model.connect(to: api(server))
        defer { model.disconnect(); Task { await server.finish() } }
        let initial = Task { await model.refresh() }
        try await completeRefresh(server, channel: "initial")
        await initial.value
        XCTAssertNotNil(model.system); XCTAssertNil(model.systemReadError)
        let failed = Task { await model.refresh() }
        try await completeRefresh(server, channel: "new", system: #"{"detail":"fixture unavailable"}"#, systemStatus: 503)
        await failed.value
        XCTAssertNil(model.system); XCTAssertNotNil(model.systemReadError)
        XCTAssertTrue(model.snapshotStale)
        XCTAssertEqual(model.channels.map(\.id), ["new"])
        XCTAssertEqual(GatewayFeedback.read(model.system, error: model.systemReadError).title, "运行状态待确认")
        let recovered = Task { await model.refresh() }
        try await completeRefresh(server, channel: "unused", channelStatus: 503)
        await recovered.value
        XCTAssertNil(model.systemReadError); XCTAssertEqual(model.system?.runtime?.phase, "ready")
        XCTAssertEqual(model.channels.map(\.id), ["new"])
        XCTAssertTrue(model.error?.hasPrefix("刷新失败：") == true)
        XCTAssertNotNil(model.channelsReadError)
        XCTAssertEqual(MenuBarPanel.menuBarCount(ready: model.ready, channels: model.channels, stale: model.snapshotStale), "1·")
        let fresh = Task { await model.refresh() }
        try await completeRefresh(server, channel: "fresh")
        await fresh.value
        XCTAssertFalse(model.snapshotStale)
    }

    @MainActor func testLateRefreshCannotOverwriteReconnectedService() async throws {
        let old = DelayedHTTP(), current = DelayedHTTP(), model = AppModel()
        defer { model.disconnect(); Task { await old.finish(); await current.finish() } }
        model.connect(to: api(old))
        let previous = Task { await model.refresh() }
        await wait(old, "/api/channels"); await wait(old, "/api/system")
        model.disconnect()
        XCTAssertFalse(model.ready); XCTAssertNil(model.api); XCTAssertNil(model.system)
        model.connect(to: api(current))
        let refresh = Task { await model.refresh() }
        try await completeRefresh(current, channel: "current")
        await refresh.value
        try await old.respond("/api/channels", json: channels("stale"))
        try await old.respond("/api/system", json: #"{"runtime":{"phase":"failed"},"mihomo_status":"down"}"#)
        await previous.value
        XCTAssertEqual(model.channels.map(\.id), ["current"])
        XCTAssertEqual(model.system?.runtime?.phase, "ready")
        XCTAssertNil(model.error)
        let oldRequests = await old.count
        XCTAssertEqual(oldRequests, 2, "Old refresh must not continue fetching adapters")
    }

    @MainActor func testRefreshAfterWriteWaitsForFreshSnapshotWhenBackgroundRefreshIsPending() async throws {
        let server = DelayedHTTP(), model = AppModel()
        defer { model.disconnect(); Task { await server.finish() } }
        model.connect(to: api(server))
        let client = try XCTUnwrap(model.api)
        let background = Task { await model.refresh() }
        await wait(server, "/api/channels"); await wait(server, "/api/system")
        let save = Task { _ = try await client.write("/api/channels/new/start"); await model.refresh() }
        await wait(server, "/api/channels/new/start")
        try await server.respond("/api/channels/new/start", json: #"{"ok":true}"#)
        try await completeRefresh(server, channel: "old-snapshot")
        await background.value
        try await completeRefresh(server, channel: "new-snapshot")
        try await save.value
        XCTAssertEqual(model.channels.map(\.id), ["new-snapshot"])
    }
}
