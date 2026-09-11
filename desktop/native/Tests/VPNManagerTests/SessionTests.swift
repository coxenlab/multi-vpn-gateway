import XCTest
import Combine
import Darwin
@testable import VPNManager

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
    @MainActor private func waitForReady(_ model: AppModel, _ value: Bool) async throws {
        let event = expectation(description: value ? "Owned core ready" : "Owned core exited")
        let observation = model.$ready.filter { $0 == value }.prefix(1).sink { _ in event.fulfill() }
        await fulfillment(of: [event], timeout: 12)
        observation.cancel()
        guard model.ready == value else { throw APIError(message: "Owned core did not reach expected state") }
    }

    @MainActor func testOwnedCoreExitReconnectPreservesDataWithoutStartingVM() async throws {
        guard let path = ProcessInfo.processInfo.environment["VPNMGR_NATIVE_LIFECYCLE_DIR"] else { throw XCTSkip("真实 core 生命周期验证需要专用进程与 Colima 替身目录") }
        guard path.hasPrefix("/tmp/vpnmgr-native-session-") else { throw APIError(message: "生命周期测试目录必须独立") }
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
        let report = try await initialAPI.importConfig(backup)
        XCTAssertEqual(report.imported, ["native-session-fixture"]); XCTAssertEqual(report.deferred, true)
        let exported = try JSONSerialization.jsonObject(with: await initialAPI.data("/api/config/export")) as? [String: Any]
        let restored = try XCTUnwrap((exported?["channels"] as? [[String: Any]])?.first)
        XCTAssertEqual(restored["config"] as? [String: String], config)
        XCTAssertEqual(restored["routing_enabled"] as? Bool, false)
        XCTAssertEqual((restored["rules"] as? [[String: Any]])?.first?["note"] as? String, "restore note")
        await model.refresh()
        let channelID = try XCTUnwrap(model.channels.first?.id)
        XCTAssertEqual(model.system?.runtime?.phase, "dormant")
        let draft = model.noteDraft(for: channelID)
        await draft.load { "" }; draft.text = "unsaved session draft"
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
        XCTAssertEqual(model.noteDraft(for: channelID).text, "unsaved session draft")
        XCTAssertFalse(model.api === initialAPI)
        XCTAssertNil(model.error)
        XCTAssertEqual(kill(try XCTUnwrap(ownedPID), SIGTERM), 0)
        try await waitForReady(model, false)
        let commands = try String(contentsOf: root.appendingPathComponent("colima-commands"), encoding: .utf8).split(separator: "\n").map(String.init)
        XCTAssertEqual(commands, ["stop vpnmgr-native-session-qa", "stop vpnmgr-native-session-qa"])
    }

    private func api(_ server: DelayedHTTP) -> LocalAPI { LocalAPI(port: 40000, transport: { try await server.send($0) }) }
    private func wait(_ server: DelayedHTTP, _ path: String) async { await fulfillment(of: [server.arrival(path)], timeout: 2) }
    private func channels(_ id: String) -> String {
        """
        [{"id":"\(id)","name":"\(id)","vpn_type":"easyconnect","server":"fixture.example","username":"fixture","status":"logged_in","configured_status":"running","login_method":"interactive","probe_url":"","routing_enabled":true,"domains":[],"ips":[]}]
        """
    }
    private func completeRefresh(_ server: DelayedHTTP, channel: String) async throws {
        await wait(server, "/api/channels"); await wait(server, "/api/system")
        try await server.respond("/api/channels", json: channels(channel))
        try await server.respond("/api/system", json: #"{"runtime":{"phase":"ready"},"mihomo_status":"running"}"#)
        await wait(server, "/api/vpn-types")
        try await server.respond("/api/vpn-types", json: "[]")
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

    @MainActor func testOldMutationCannotClearNewBusyStateOrReportSuccessOrFailure() async throws {
        for oldStatus in [200, 503] {
            let old = DelayedHTTP(), current = DelayedHTTP(), model = AppModel()
            defer { model.disconnect(); Task { await old.finish(); await current.finish() } }
            model.connect(to: api(old))
            let previous = Task { await model.perform("/api/channels", key: "create", success: "old success") }
            await wait(old, "/api/channels")
            model.connect(to: api(current))
            let active = Task { await model.perform("/api/channels", key: "create", success: "new success") }
            await wait(current, "/api/channels")
            try await old.respond("/api/channels", json: oldStatus == 200 ? #"{"id":"old"}"# : #"{"error":"old failure"}"#, status: oldStatus)
            let previousSucceeded = await previous.value
            XCTAssertFalse(previousSucceeded)
            XCTAssertTrue(model.busy.contains("create")); XCTAssertNil(model.createdChannelID)
            XCTAssertNil(model.message); XCTAssertNil(model.error)
            try await current.respond("/api/channels", json: #"{"id":"new"}"#)
            try await completeRefresh(current, channel: "new")
            let currentSucceeded = await active.value
            XCTAssertTrue(currentSucceeded); XCTAssertEqual(model.createdChannelID, "new")
            XCTAssertEqual(model.message, "new success"); XCTAssertFalse(model.busy.contains("create"))
        }
    }

    @MainActor func testMutationWaitsForFreshSnapshotWhenBackgroundRefreshIsPending() async throws {
        let server = DelayedHTTP(), model = AppModel()
        defer { model.disconnect(); Task { await server.finish() } }
        model.connect(to: api(server))
        let background = Task { await model.refresh() }
        await wait(server, "/api/channels"); await wait(server, "/api/system")
        let save = Task { await model.perform("/api/channels/new/start", key: "new") }
        await wait(server, "/api/channels/new/start")
        try await server.respond("/api/channels/new/start", json: #"{"ok":true}"#)
        try await completeRefresh(server, channel: "old-snapshot")
        await background.value
        try await completeRefresh(server, channel: "new-snapshot")
        let success = await save.value
        XCTAssertTrue(success)
        XCTAssertEqual(model.channels.map(\.id), ["new-snapshot"])
        XCTAssertFalse(model.busy.contains("new"))
    }

    @MainActor func testLateImageStatusDoesNotRemoveNewImportOrChangeDownloadProgress() async throws {
        let old = DelayedHTTP(), current = DelayedHTTP(), model = AppModel()
        defer { model.disconnect(); Task { await old.finish(); await current.finish() } }
        let preview = ImageArchivePreview(images: [], bytes: 1, sha256: "fixture")
        model.connect(to: api(old))
        model.importTicket = ImageImportTicket(id: "old", status: "loading", progress: nil, error: nil, preview: preview)
        let previous = Task { await model.refreshImageImport() }
        await wait(old, "/api/images/imports/old")
        model.connect(to: api(current))
        XCTAssertNil(model.importTicket); XCTAssertNotNil(model.importError)
        model.importTicket = ImageImportTicket(id: "new", status: "preview", progress: nil, error: nil, preview: preview)
        let active = Task { await model.refreshImageImport() }
        await wait(current, "/api/images/imports/new")
        try await old.respond("/api/images/imports/old", json: #"{"error":"expired"}"#, status: 404)
        await previous.value
        XCTAssertEqual(model.importTicket?.id, "new"); XCTAssertTrue(model.busy.contains("__image-refresh"))
        try await current.respond("/api/images/imports/new", json: #"{"id":"new","status":"done","preview":{"images":[],"bytes":1,"sha256":"fixture"}}"#)
        await active.value
        XCTAssertEqual(model.importTicket?.status, "done"); XCTAssertNil(model.importError)

        let download = Task { await model.downloadImage(ImageEntry(image: "vpnmgr/oss-vpn:latest", title: "fixture", kind: "pull", present: false)) }
        await wait(current, "/api/preflight/fix/pull_image")
        try await current.respond("/api/preflight/fix/pull_image", json: #"{"task_id":"download"}"#)
        await wait(current, "/api/preflight/fix/download")
        model.disconnect()
        try await current.respond("/api/preflight/fix/download", json: #"{"status":"done","progress":"old progress"}"#)
        await download.value
        XCTAssertTrue(model.imageProgress.isEmpty); XCTAssertNil(model.message)
    }
}
