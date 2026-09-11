import XCTest
import WebKit
@testable import VPNManager

final class WebWorkspaceTests: XCTestCase {
    @MainActor func testManagementOriginCannotMoveToAnotherPortOrRemoteSite() {
        let controller = WebWorkspaceController()
        controller.connect(URL(string: "http://127.0.0.1:41234/")!, visible: false)
        defer { controller.webView.stopLoading() }
        XCTAssertTrue(controller.isLocal(URL(string: "http://127.0.0.1:41234/channel.html?id=c1")!))
        XCTAssertFalse(controller.isLocal(URL(string: "http://127.0.0.1:41235/")!))
        XCTAssertFalse(controller.isLocal(URL(string: "https://example.invalid/")!))
        controller.connect(URL(string: "http://127.0.0.1:41235/")!, visible: true)
        XCTAssertEqual(controller.origin?.port, 41234)
    }

    @MainActor func testHidingPreservesDraftAndRendererFailureKeepsSessionIdentity() async throws {
        let controller = WebWorkspaceController()
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 920, height: 620), styleMask: [.titled], backing: .buffered, defer: false)
        window.contentView = controller.webView
        defer { controller.webView.stopLoading(); window.contentView = nil }
        controller.connect(URL(string: "http://127.0.0.1:41234/")!, visible: false)
        controller.webView.loadHTMLString("<input id='draft' value='unsaved fixture'><script>window.loaded=true</script>", baseURL: URL(string: "http://127.0.0.1:41234/"))
        let deadline = Date().addingTimeInterval(10)
        while (try? await controller.webView.evaluateJavaScript("window.loaded === true")) as? Bool != true {
            guard Date() < deadline else { XCTFail("WebKit fixture failed to load"); return }
            try await Task.sleep(for: .milliseconds(50))
        }
        controller.setVisible(false)
        let hidden = try await controller.webView.evaluateJavaScript("document.hidden && document.visibilityState === 'hidden'")
        XCTAssertEqual(hidden as? Bool, true)
        let draft = try await controller.webView.evaluateJavaScript("document.getElementById('draft').value")
        XCTAssertEqual(draft as? String, "unsaved fixture")
        let currentURL = controller.webView.url
        controller.webViewWebContentProcessDidTerminate(controller.webView)
        XCTAssertNotNil(controller.error)
        XCTAssertEqual(controller.webView.url, currentURL, "renderer failure must not silently repeat navigation or writes")
    }
}
