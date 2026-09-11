import XCTest
@testable import VPNManager

final class PageRequestTests: XCTestCase {
    private func api(_ server: DelayedHTTP) -> LocalAPI { LocalAPI(port: 40000, transport: { try await server.send($0) }) }
    private func wait(_ server: DelayedHTTP, _ path: String) async { await fulfillment(of: [server.arrival(path)], timeout: 2) }

    @MainActor func testLatePageSuccessOrFailureCannotReplaceNewDataOrClearLoading() async throws {
        for oldStatus in [200, 503] {
            let server = DelayedHTTP(), model = AppModel(), request = PageRequest()
            defer { model.disconnect(); Task { await server.finish() } }
            model.connect(to: api(server))
            var displayed = "initial"
            let first = Task { await request.run(model: model, operation: { try await $0.data("/old") }) { displayed = String(decoding: $0, as: UTF8.self) } }
            await wait(server, "/old")
            let second = Task { await request.run(model: model, operation: { try await $0.data("/current") }) { displayed = String(decoding: $0, as: UTF8.self) } }
            await wait(server, "/current")
            try await server.respond("/old", json: #"{"error":"stale"}"#, status: oldStatus)
            let acceptedOld = await first.value
            XCTAssertFalse(acceptedOld); XCTAssertEqual(displayed, "initial")
            XCTAssertTrue(request.loading); XCTAssertNil(request.error)
            try await server.respond("/current", json: "current")
            let acceptedCurrent = await second.value
            XCTAssertTrue(acceptedCurrent); XCTAssertEqual(displayed, "current")
            XCTAssertFalse(request.loading); XCTAssertNil(request.error)
        }
    }

    @MainActor func testSuspendedPageIgnoresPendingResponsesAndFollowUpLoads() async throws {
        let server = DelayedHTTP(), model = AppModel(), request = PageRequest()
        defer { model.disconnect(); Task { await server.finish() } }
        model.connect(to: api(server))
        let pending = Task { await request.run(model: model, operation: { try await $0.data("/pending") }) { _ in XCTFail("Hidden page received old data") } }
        await wait(server, "/pending")
        request.suspend()
        try await server.respond("/pending", json: #"{"error":"late"}"#, status: 500)
        let accepted = await pending.value
        XCTAssertFalse(accepted); XCTAssertFalse(request.loading); XCTAssertNil(request.error)
        let followed = await request.run(model: model, operation: { _ in XCTFail("Hidden page started a follow-up request"); return "" }) { _ in XCTFail() }
        XCTAssertFalse(followed)
        request.activate()
        var text = ""
        let reloaded = await request.run(model: model, operation: { _ in "visible" }) { text = $0 }
        XCTAssertTrue(reloaded); XCTAssertEqual(text, "visible")
    }

    @MainActor func testPauseCancellationAndCoreReplacementDoNotPublishLateResults() async throws {
        for replaceCore in [false, true] {
            let server = DelayedHTTP(), model = AppModel(), request = PageRequest()
            defer { model.disconnect(); Task { await server.finish() } }
            model.connect(to: api(server))
            let pending = Task { await request.run(model: model, operation: { try await $0.data("/pending") }) { _ in XCTFail("Cancelled/session-old data became visible") } }
            await wait(server, "/pending")
            if replaceCore { model.connect(to: LocalAPI(port: 1)) }
            else { request.cancel() }
            try await server.respond("/pending", json: "old")
            let accepted = await pending.value
            XCTAssertFalse(accepted); XCTAssertFalse(request.loading); XCTAssertNil(request.error)
        }
    }
}
