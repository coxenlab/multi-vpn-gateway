import XCTest
@testable import VPNManager

final class BackupImportTests: XCTestCase {
    private let file = URL(fileURLWithPath: "/unused-fixture.json")
    private func document(_ count: Int = 1) -> BackupDocument {
        BackupDocument(data: Data(#"{"kind":"vpnmgr-export","channels":[]}"#.utf8), channelCount: count, ruleCount: 0, names: [])
    }
    private func client(_ server: DelayedHTTP) -> LocalAPI {
        LocalAPI(port: 40000, transport: { request in
            if request.url!.path == "/api/config/import" { return try await server.send(request) }
            let text = request.url!.path == "/api/system" ? "{}" : "[]"
            return (Data(text.utf8), HTTPURLResponse(url: request.url!, statusCode: 200, httpVersion: nil, headerFields: [:])!)
        })
    }
    private func wait(_ server: DelayedHTTP, _ path: String) async { await fulfillment(of: [server.arrival(path)], timeout: 2) }

    func testBoundedBackupReaderValidatesBeforeConfirmation() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("vpnmgr-backup-test-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: root) }
        let file = root.appendingPathComponent("large.json")
        let bytes = try JSONSerialization.data(withJSONObject: ["kind":"vpnmgr-export", "version":1, "channels":[["name":"通道甲","config":["data":String(repeating:"x",count:3*1024*1024)],"rules":[["pattern":"test.example"]]]]])
        try bytes.write(to: file)
        let document = try await BackupDocument.load(file)
        XCTAssertEqual(document.data, bytes)
        XCTAssertEqual(document.channelCount, 1); XCTAssertEqual(document.ruleCount, 1); XCTAssertEqual(document.names, ["通道甲"])
        for invalid in ["", "not-json", #"{"kind":"wrong","channels":[]}"#, #"{"kind":"vpnmgr-export","channels":{}}"#] {
            try Data(invalid.utf8).write(to: file)
            do { _ = try await BackupDocument.load(file); XCTFail("Invalid backup accepted") } catch { XCTAssertTrue(error is APIError) }
        }
        let handle = try FileHandle(forWritingTo: file); try handle.truncate(atOffset: UInt64(BackupDocument.maximumBytes + 1)); try handle.close()
        do { _ = try await BackupDocument.load(file); XCTFail("Oversize backup accepted") } catch { XCTAssertTrue(error.localizedDescription.contains("16 MiB")) }
        let cancelled = Task { try await BackupDocument.load(file) }; cancelled.cancel()
        do { _ = try await cancelled.value; XCTFail("Cancelled backup read succeeded") } catch { XCTAssertTrue(error is CancellationError) }
    }

    @MainActor func testDuplicateSubmitAndPartialImportKeepDetailedReport() async throws {
        let server = DelayedHTTP(), state = BackupImportState(), model = AppModel(), document = document()
        defer { model.disconnect(); Task { await server.finish() } }
        model.connect(to: client(server))
        await state.prepare(file: file, model: model, loader: { _ in document })
        let submitted = Task { await state.submit(model: model) }
        await wait(server, "/api/config/import")
        await state.submit(model: model)
        let count = await server.count; XCTAssertEqual(count, 1)
        XCTAssertTrue(state.submitting)
        try await server.respond("/api/config/import", json: #"{"ok":true,"imported":["A"],"skipped":[{"name":"A","reason":"规则格式错误"},{"name":"B","reason":"同名通道已存在"}],"rules_applied":false}"#)
        await submitted.value
        XCTAssertFalse(state.submitting); XCTAssertNil(state.document); XCTAssertNil(state.error)
        XCTAssertEqual(state.report?.imported, ["A"]); XCTAssertEqual(state.report?.skipped.count, 2)
        XCTAssertEqual(state.report?.skipped.first?.reason, "规则格式错误"); XCTAssertEqual(state.report?.pending, true)
        await state.submit(model: model)
        let finalCount = await server.count; XCTAssertEqual(finalCount, 1)
    }

    @MainActor func testUnavailableCoreShowsReasonWithoutReadingBackup() async {
        let state = BackupImportState(), model = AppModel()
        await state.prepare(file: file, model: model, loader: { _ in
            XCTFail("Must not read backup before a core is available"); throw CancellationError()
        })
        XCTAssertNil(state.document); XCTAssertFalse(state.loading)
        XCTAssertTrue(state.error?.contains("尚未就绪") == true)
    }

    @MainActor func testCancelledReadAndOldCoreRepliesCannotReplaceNewState() async throws {
        let server = DelayedHTTP(), state = BackupImportState(), model = AppModel(), oldDocument = document(), nextDocument = document(2)
        defer { model.disconnect(); Task { await server.finish() } }
        model.connect(to: client(server))
        let reading = Task { await state.prepare(file: file, model: model, loader: { _ in
            _ = try await server.send(URLRequest(url: URL(string: "http://127.0.0.1/read")!)); return oldDocument
        }) }
        await wait(server, "/read"); state.cancel()
        await state.prepare(file: file, model: model, loader: { _ in nextDocument })
        try await server.respond("/read", json: "{}"); await reading.value
        XCTAssertEqual(state.document?.channelCount, 2)
        let submitted = Task { await state.submit(model: model) }
        await wait(server, "/api/config/import")
        model.connect(to: LocalAPI(port: 40001, transport: { request in
            XCTFail("Old import must not refresh new core")
            return (Data("[]".utf8), HTTPURLResponse(url: request.url!, statusCode: 200, httpVersion: nil, headerFields: [:])!)
        }))
        state.connectionChanged()
        try await server.respond("/api/config/import", json: #"{"ok":true,"imported":["late"],"skipped":[]}"#)
        await submitted.value
        XCTAssertNil(state.document); XCTAssertNil(state.report); XCTAssertFalse(state.submitting)
        XCTAssertTrue(state.error?.contains("连接已变化") == true)
    }

    @MainActor func testUnconfirmedImportDoesNotOfferBlindRepeat() async throws {
        for status in [200, 500] {
            let server = DelayedHTTP(), model = AppModel(), state = BackupImportState(), document = document()
            defer { model.disconnect(); Task { await server.finish() } }
            model.connect(to: client(server)); await state.prepare(file: file, model: model, loader: { _ in document })
            let submitted = Task { await state.submit(model: model) }; await wait(server, "/api/config/import")
            try await server.respond("/api/config/import", json: #"{"ok":false,"imported":[],"skipped":[],"error":"injected failure"}"#, status: status)
            await submitted.value
            XCTAssertNil(state.report); XCTAssertNil(state.document)
            XCTAssertTrue(state.error?.contains("核对结果") == true)
        }
    }
}
