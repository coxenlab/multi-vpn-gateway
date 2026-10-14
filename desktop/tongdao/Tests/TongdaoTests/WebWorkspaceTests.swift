import XCTest
import WebKit
@testable import Tongdao

final class WebWorkspaceTests: XCTestCase {
    @MainActor func testMissingBridgeDoesNotQueueEveryLaterJump() async throws {
        let controller = WebWorkspaceController()
        let window = try await loadFixture(controller, "<p>Bridge unavailable initially</p>")
        defer { controller.webView.stopLoading(); window.contentView = nil }
        controller.open("#/channels/first")
        try await Task.sleep(for: .milliseconds(100))
        _ = try await controller.webView.evaluateJavaScript("window.opened=[]; window.__tongdaoOpen = hash => window.opened.push(hash); true")
        controller.open("#/settings")
        try await Task.sleep(for: .milliseconds(100))
        var opened = try await controller.webView.evaluateJavaScript("window.opened") as? [String]
        XCTAssertEqual(opened, ["#/settings"])
        controller.webView(controller.webView, didFinish: nil)
        try await Task.sleep(for: .milliseconds(100))
        opened = try await controller.webView.evaluateJavaScript("window.opened") as? [String]
        XCTAssertEqual(opened, ["#/settings", "#/channels/first"])
        controller.webView(controller.webView, didFinish: nil)
        try await Task.sleep(for: .milliseconds(100))
        let final = try await controller.webView.evaluateJavaScript("window.opened") as? [String]
        XCTAssertEqual(final, opened)
    }

    @MainActor func testManagementOriginCannotMoveToAnotherPortOrRemoteSite() {
        let controller = WebWorkspaceController()
        controller.connect(URL(string: "http://127.0.0.1:41234/")!, visible: false)
        defer { controller.webView.stopLoading() }
        XCTAssertTrue(controller.isLocal(URL(string: "http://127.0.0.1:41234/#/channels/c1")!))
        XCTAssertFalse(controller.isLocal(URL(string: "http://127.0.0.1:41235/")!))
        XCTAssertFalse(controller.isLocal(URL(string: "https://example.invalid/")!))
        controller.connect(URL(string: "http://127.0.0.1:41235/")!, visible: true)
        XCTAssertEqual(controller.origin?.port, 41234)
    }

    @MainActor private func loadFixture(_ controller: WebWorkspaceController, _ html: String) async throws -> NSWindow {
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 920, height: 620), styleMask: [.titled], backing: .buffered, defer: false)
        window.contentView = controller.webView
        controller.connect(URL(string: "http://127.0.0.1:41234/")!, visible: false)
        controller.webView.loadHTMLString(html + "<script>window.loaded=true</script>", baseURL: URL(string: "http://127.0.0.1:41234/"))
        let deadline = Date().addingTimeInterval(10)
        while (try? await controller.webView.evaluateJavaScript("window.loaded === true")) as? Bool != true {
            guard Date() < deadline else { throw APIError(message: "WebKit fixture failed to load") }
            try await Task.sleep(for: .milliseconds(50))
        }
        return window
    }

    @MainActor func testHidingPreservesDraftAndRendererFailureKeepsSessionIdentity() async throws {
        let controller = WebWorkspaceController()
        let window = try await loadFixture(controller, "<input id='draft' value='unsaved fixture'>")
        defer { controller.webView.stopLoading(); window.contentView = nil }
        controller.setVisible(false)
        let hidden = try await controller.webView.evaluateJavaScript("document.hidden && document.visibilityState === 'hidden'")
        XCTAssertEqual(hidden as? Bool, true)
        let bridge = try await controller.webView.evaluateJavaScript("typeof window.__tongdaoSetVisible === 'function' && typeof window.__vpnmgrSetVisible === 'undefined'")
        XCTAssertEqual(bridge as? Bool, true)
        let draft = try await controller.webView.evaluateJavaScript("document.getElementById('draft').value")
        XCTAssertEqual(draft as? String, "unsaved fixture")
        let currentURL = controller.webView.url
        controller.webViewWebContentProcessDidTerminate(controller.webView)
        XCTAssertNotNil(controller.error)
        XCTAssertEqual(controller.webView.url, currentURL, "renderer failure must not silently repeat navigation or writes")
    }

    @MainActor func testVersionIsInjectedAndMenuActionsRouteInsideThePage() async throws {
        let controller = WebWorkspaceController(appVersion: "0.1.0 \"fixture\"")
        let window = try await loadFixture(controller, "<script>window.opened=[]; window.__tongdaoOpen = hash => window.opened.push(hash)</script>")
        defer { controller.webView.stopLoading(); window.contentView = nil }
        let version = try await controller.webView.evaluateJavaScript("window.__tongdaoAppVersion")
        XCTAssertEqual(version as? String, "0.1.0 \"fixture\"")
        let before = controller.webView.url
        controller.open("?sheet=add"); controller.open("#/settings")
        let deadline = Date().addingTimeInterval(5)
        var opened: [String] = []
        while opened.count < 2 && Date() < deadline {
            opened = (try? await controller.webView.evaluateJavaScript("window.opened")) as? [String] ?? []
            try await Task.sleep(for: .milliseconds(20))
        }
        XCTAssertEqual(opened, ["?sheet=add", "#/settings"])
        XCTAssertEqual(controller.webView.url, before, "menu actions must not reload or navigate the web view")
        XCTAssertEqual(WebWorkspaceController.versionScript("开发版"), #"window.__tongdaoAppVersion = "开发版";"#)
    }

    @MainActor func testJumpRequestedBeforeThePageLoadsRunsOnceAfterLoading() async throws {
        let controller = WebWorkspaceController()
        controller.open("#/channels/c1?tab=login")
        let window = try await loadFixture(controller, "<script>window.opened=[]; window.__tongdaoOpen = hash => window.opened.push(hash)</script>")
        defer { controller.webView.stopLoading(); window.contentView = nil }
        let deadline = Date().addingTimeInterval(5)
        var opened: [String] = []
        while opened.isEmpty && Date() < deadline {
            opened = (try? await controller.webView.evaluateJavaScript("window.opened")) as? [String] ?? []
            try await Task.sleep(for: .milliseconds(20))
        }
        XCTAssertEqual(opened, ["#/channels/c1?tab=login"])
        controller.webView(controller.webView, didFinish: nil)
        try await Task.sleep(for: .milliseconds(200))
        opened = (try await controller.webView.evaluateJavaScript("window.opened")) as? [String] ?? []
        XCTAssertEqual(opened, ["#/channels/c1?tab=login"], "a delivered jump is not repeated by a later load")
    }
}
