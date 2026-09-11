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
    @MainActor func testOfflineUpgradeStopsOnlyOwnedCoreAndRestoresConfiguration() async throws {
        let inherited = ProcessInfo.processInfo.environment
        guard let executable = inherited["VPNMGR_NATIVE_CORE_PATH"], let original = inherited["VPNMGR_NATIVE_UPGRADE_SOURCE"] else { throw XCTSkip("升级闭环需要独立 core 与合成源数据") }
        guard original.hasPrefix("/tmp/vpnmgr-native-qa-") else { throw APIError(message: "升级测试仅允许合成源目录") }
        let files = FileManager.default, root = files.temporaryDirectory.appendingPathComponent("vpnmgr-native-switch-" + UUID().uuidString)
        let bin = root.appendingPathComponent("bin"), source = root.appendingPathComponent("source"), data = root.appendingPathComponent("current")
        try files.createDirectory(at: bin, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
        try files.copyItem(at: URL(fileURLWithPath: original), to: source)
        let originalDatabase = try Data(contentsOf: source.appendingPathComponent("vpnmgr.db"))
        let listing = root.appendingPathComponent("listing"), commands = root.appendingPathComponent("commands"), pidFile = root.appendingPathComponent("pid")
        let profile = "vpnmgr-native-switch-qa"
        try Data("{\"name\":\"\(profile)\",\"status\":\"Stopped\"}\n".utf8).write(to: listing)
        let colima = bin.appendingPathComponent("colima")
        try """
        #!/bin/sh
        printf '%s\\n' "$*" >> "$VPNMGR_UPGRADE_TEST_COMMANDS"
        case "$1" in
          list) cat "$VPNMGR_UPGRADE_TEST_LISTING" ;;
          stop) exit 0 ;;
          *) exit 73 ;;
        esac
        """.write(to: colima, atomically: true, encoding: .utf8)
        try files.setAttributes([.posixPermissions: 0o700], ofItemAtPath: colima.path)
        let wrapper = bin.appendingPathComponent("core")
        let quotedExecutable = "'" + executable.replacingOccurrences(of: "'", with: "'\\''") + "'"
        try """
        #!/bin/sh
        if [ "$#" -eq 0 ]; then printf '%s\\n' "$$" > "$VPNMGR_UPGRADE_TEST_PID"; fi
        exec \(quotedExecutable) "$@"
        """.write(to: wrapper, atomically: true, encoding: .utf8)
        try files.setAttributes([.posixPermissions: 0o700], ofItemAtPath: wrapper.path)
        var environment = inherited.filter { !$0.key.hasPrefix("MIHOMO_") && $0.key != "UI_PORT" }
        environment["VPNMGR_CORE_PATH"] = wrapper.path; environment["DATA_DIR"] = data.path
        environment["VPNMGR_DEV_MODE"] = "1"; environment["VPNMGR_VM_PROFILE"] = profile
        environment["VPN_NET"] = "vpnmgr_native_switch_qa"; environment["PATH"] = bin.path + ":" + (inherited["PATH"] ?? "/usr/bin:/bin")
        environment["VPNMGR_UPGRADE_TEST_COMMANDS"] = commands.path; environment["VPNMGR_UPGRADE_TEST_LISTING"] = listing.path
        environment["VPNMGR_UPGRADE_TEST_PID"] = pidFile.path
        let model = AppModel(launchEnvironment: environment)
        func ownedPID() throws -> pid_t { try XCTUnwrap(pid_t(String(contentsOf: pidFile).trimmingCharacters(in: .whitespacesAndNewlines))) }
        defer { if model.ready, let pid = try? ownedPID() { _ = kill(pid, SIGTERM) } }
        model.launch(); try await waitForReady(model, true); await model.refresh()
        XCTAssertTrue(model.channels.isEmpty)
        let candidate = root.appendingPathComponent("review")
        await model.prepareUpgrade(from: source, to: candidate)
        XCTAssertEqual(model.upgradeReport?.version, 2); XCTAssertNil(model.upgradeError)
        let draft = model.noteDraft(for: "upgrade-fixture"); await draft.load { "" }; draft.text = "unsaved fixture"
        await model.switchUpgrade(.activate)
        XCTAssertTrue(model.ready); XCTAssertTrue(model.upgradeError?.contains("尚未保存") == true)
        draft.text = ""
        // Direct activation while this data directory is owned must fail before any VM call.
        let context = OfflineLaunchContext(executable: wrapper, environment: environment)
        do {
            let _: UpgradeSwitchStatus = try await Task.detached { try runOfflineCore(context: context, arguments: ["--activate-upgrade", candidate.path], as: UpgradeSwitchStatus.self) }.value
            XCTFail("An active data owner must block switching")
        } catch { XCTAssertTrue(error.localizedDescription.contains("正在使用")) }
        XCTAssertFalse(files.fileExists(atPath: commands.path))

        try Data("{\"name\":\"\(profile)\",\"status\":\"Running\"}\n".utf8).write(to: listing)
        await model.switchUpgrade(.activate)
        XCTAssertFalse(model.ready); XCTAssertTrue(model.upgradeError?.contains("仍在使用") == true)
        XCTAssertFalse(files.fileExists(atPath: data.appendingPathComponent("upgrade-activation.json").path))
        try Data("{\"name\":\"\(profile)\",\"status\":\"Stopped\"}\n".utf8).write(to: listing)
        await model.switchUpgrade(.activate); try await waitForReady(model, true); await model.refresh()
        XCTAssertNil(model.upgradeError); XCTAssertEqual(model.upgradeStatus?.phase, "active")
        XCTAssertEqual(model.channels.count, 1); XCTAssertEqual(model.system?.runtime?.phase, "dormant")
        let retainedOld = try XCTUnwrap(model.upgradeStatus?.retained_directory)
        XCTAssertTrue(files.fileExists(atPath: retainedOld + "/vpnmgr.db"))
        await model.switchUpgrade(.rollback); try await waitForReady(model, true); await model.refresh()
        XCTAssertNil(model.upgradeError); XCTAssertEqual(model.upgradeStatus?.phase, "rolled_back")
        XCTAssertTrue(model.channels.isEmpty); XCTAssertEqual(model.system?.runtime?.phase, "dormant")
        XCTAssertTrue(try Data(contentsOf: source.appendingPathComponent("vpnmgr.db")) == originalDatabase)
        await model.switchUpgrade(.finish); try await waitForReady(model, true)
        XCTAssertNil(model.upgradeStatus); XCTAssertNotNil(model.completedUpgrade)
        XCTAssertTrue(files.fileExists(atPath: try XCTUnwrap(model.completedUpgrade?.retained_directory) + "/vpnmgr.db"))
        XCTAssertEqual(kill(try ownedPID(), SIGTERM), 0); try await waitForReady(model, false)
        let calls = try String(contentsOf: commands).split(separator: "\n").map(String.init)
        XCTAssertTrue(calls.contains("list --json"))
        XCTAssertTrue(calls.allSatisfy { $0 == "list --json" || $0 == "stop " + profile })
        try files.removeItem(at: root)
    }

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

    @MainActor func testRuntimeStartKeepsProgressAndRetriesOnlyAfterExplicitAction() async throws {
        let server = DelayedHTTP(), model = AppModel()
        model.connect(to: api(server))
        defer { model.disconnect(); Task { await server.finish() } }
        func snapshot(_ runtime: String) async throws {
            await wait(server, "/api/channels"); await wait(server, "/api/system")
            try await server.respond("/api/channels", json: "[]")
            try await server.respond("/api/system", json: "{\"runtime\":\(runtime)}")
            await wait(server, "/api/vpn-types"); try await server.respond("/api/vpn-types", json: "[]")
        }
        let start = Task { await model.perform("/api/runtime/start", key: "__runtime") }
        await wait(server, "/api/runtime/start")
        let duplicate = await model.perform("/api/runtime/start", key: "__runtime")
        XCTAssertFalse(duplicate)
        let refresh = Task { await model.refresh() }
        try await snapshot(#"{"phase":"starting","detail":"正在下载运行环境文件…","progress_age_seconds":90}"#)
        await refresh.value
        XCTAssertTrue(model.busy.contains("__runtime")); XCTAssertTrue(model.system?.runtime?.quietPreparation == true)
        XCTAssertNil(model.error)
        try await server.respond("/api/runtime/start", json: #"{"detail":"磁盘空间不足"}"#, status: 503)
        try await snapshot(#"{"phase":"failed","detail":"正在下载旧阶段","error":"磁盘空间不足"}"#)
        let completed = await start.value
        XCTAssertFalse(completed); XCTAssertFalse(model.busy.contains("__runtime"))
        // Dismissing the transient error keeps the durable server reason and manual retry.
        model.error = nil
        XCTAssertEqual(model.system?.runtime?.notice, "磁盘空间不足")
        XCTAssertTrue(model.system?.runtime?.canConnect == true)
        let beforeRetry = await server.count
        XCTAssertEqual(beforeRetry, 7) // One write and two read snapshots; no automatic restart.
        let retry = Task { await model.perform("/api/runtime/start", key: "__runtime", success: "运行环境已连接") }
        await wait(server, "/api/runtime/start")
        try await server.respond("/api/runtime/start", json: #"{"ok":true}"#)
        try await snapshot(#"{"phase":"ready","detail":"运行环境已就绪"}"#)
        let retried = await retry.value
        XCTAssertTrue(retried); XCTAssertEqual(model.message, "运行环境已连接")
        XCTAssertTrue(model.system?.runtime?.ready == true); XCTAssertFalse(model.busy.contains("__runtime"))
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

    @MainActor func testDownloadQueryFailureResumesOriginalTaskWithoutAnotherPost() async throws {
        let server = DelayedHTTP(), model = AppModel(imagePollInterval: .milliseconds(1))
        defer { model.disconnect(); Task { await server.finish() } }
        model.connect(to: api(server))
        let image = ImageEntry(image: "hagb/docker-atrust:latest", title: "fixture", kind: "pull", present: false)
        let path = "/api/preflight/fix/pull-fixture"
        let download = Task { await model.downloadImage(image) }
        await wait(server, "/api/preflight/fix/pull_image")
        try await server.respond("/api/preflight/fix/pull_image", json: #"{"task_id":"pull-fixture"}"#)
        for _ in 0..<5 {
            await wait(server, path)
            try await server.respond(path, json: #"{"error":"fixture unavailable"}"#, status: 503)
        }
        await download.value
        XCTAssertFalse(model.busy.contains("image-" + image.id))
        XCTAssertEqual(model.imageTasks[image.id], "pull-fixture")
        XCTAssertNotNil(model.error)
        let resume = Task { await model.downloadImage(image) }
        await wait(server, path)
        try await server.respond(path, json: #"{"status":"done","progress":"complete"}"#)
        await resume.value
        let count = await server.count
        XCTAssertEqual(count, 7) // One POST, five failed GETs, one resumed GET.
        XCTAssertNil(model.imageTasks[image.id]); XCTAssertNil(model.error)
        XCTAssertEqual(model.imageProgress[image.id], "已准备")
    }

    @MainActor func testDownloadTimeoutKeepsTicketAndExpiredRecordClearsIt() async throws {
        let server = DelayedHTTP(), model = AppModel(imagePollInterval: .milliseconds(1), imageWaitLimit: .milliseconds(50))
        defer { model.disconnect(); Task { await server.finish() } }
        model.connect(to: api(server)); model.visible = false
        let image = ImageEntry(image: "hagb/docker-atrust:latest", title: "fixture", kind: "pull", present: false)
        let download = Task { await model.downloadImage(image) }
        await wait(server, "/api/preflight/fix/pull_image")
        try await server.respond("/api/preflight/fix/pull_image", json: #"{"task_id":"expired"}"#)
        await download.value
        XCTAssertEqual(model.imageTasks[image.id], "expired")
        XCTAssertTrue(model.error?.contains("超时") == true)
        XCTAssertFalse(model.busy.contains("image-" + image.id))
        model.visible = true
        let resume = Task { await model.downloadImage(image) }
        await wait(server, "/api/preflight/fix/expired")
        try await server.respond("/api/preflight/fix/expired", json: #"{"error":"unknown task"}"#, status: 404)
        await resume.value
        XCTAssertNil(model.imageTasks[image.id])
        XCTAssertTrue(model.error?.contains("已失效") == true)
        let count = await server.count
        XCTAssertEqual(count, 2)
    }

    @MainActor func testDownloadTerminalFailureAllowsExplicitRetry() async throws {
        let server = DelayedHTTP(), model = AppModel()
        defer { model.disconnect(); Task { await server.finish() } }
        model.connect(to: api(server))
        let image = ImageEntry(image: "hagb/docker-atrust:latest", title: "fixture", kind: "pull", present: false)
        let download = Task { await model.downloadImage(image) }
        await wait(server, "/api/preflight/fix/pull_image")
        try await server.respond("/api/preflight/fix/pull_image", json: #"{"task_id":"failed"}"#)
        await wait(server, "/api/preflight/fix/failed")
        try await server.respond("/api/preflight/fix/failed", json: #"{"status":"error","error":"fixture source failed"}"#)
        await download.value
        XCTAssertNil(model.imageTasks[image.id])
        XCTAssertEqual(model.imageProgress[image.id], "fixture source failed")
        XCTAssertFalse(model.busy.contains("image-" + image.id))
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
