import XCTest
import SwiftUI
@testable import VPNManager

final class ContractTests: XCTestCase {
    func testDecodesActualImagePreview() throws {
        let ticket = try JSONDecoder().decode(ImageImportTicket.self, from: fixture("native-image-import.json"))
        XCTAssertEqual(ticket.status, "preview")
        XCTAssertEqual(ticket.preview.images.first?.tags, ["vpnmgr/oss-vpn:latest"])
        XCTAssertEqual(ticket.preview.images.first?.architecture, "arm64")
        XCTAssertEqual(ticket.preview.sha256.count, 64)
    }
    private func fixture(_ name: String) throws -> Data {
        guard let directory = ProcessInfo.processInfo.environment["VPNMGR_NATIVE_FIXTURE_DIR"] else {
            throw XCTSkip("真实 core API 合同验证需要隔离夹具输出目录")
        }
        return try Data(contentsOf: URL(fileURLWithPath: directory).appendingPathComponent(name))
    }
    func testDecodesActualCoreChannelsAndAdapterContracts() throws {
        let channels = try JSONDecoder().decode([Channel].self, from: fixture("native-channels.json"))
        let adapters = try JSONDecoder().decode([Adapter].self, from: fixture("native-adapters.json"))
        let system = try JSONDecoder().decode(SystemStatus.self, from: fixture("native-system.json"))
        XCTAssertEqual(channels.count, 1)
        XCTAssertEqual(channels[0].rules[0].pattern, "native.example")
        XCTAssertEqual(channels[0].rules[0].enabled, 1)
        XCTAssertEqual(system.runtime?.phase, "dormant")
        XCTAssertTrue(adapters.contains { $0.key == "easyconnect" && $0.inputs.contains { $0.secret == true } })
        var channel = channels[0]
        channel.stop_pending = true; channel.status = "logged_in"
        XCTAssertTrue(channel.needsStart)
        XCTAssertEqual(channel.statusLabel(runtime: system.runtime), "停用待确认")
    }
    @MainActor func testNativeWorkspaceLaysOutWithoutOpeningAWindow() throws {
        let model = AppModel()
        model.channels = try JSONDecoder().decode([Channel].self, from: fixture("native-channels.json"))
        model.adapters = try JSONDecoder().decode([Adapter].self, from: fixture("native-adapters.json"))
        model.system = try JSONDecoder().decode(SystemStatus.self, from: fixture("native-system.json"))
        model.ready = true
        let view = NSHostingView(rootView: WorkspaceView(initialSelection: model.channels[0].id).environmentObject(model).environment(\.colorScheme, .light).background(Color(nsColor: .windowBackgroundColor)))
        view.frame = NSRect(x: 0, y: 0, width: 1100, height: 760)
        let window = NSWindow(contentRect: view.frame, styleMask: [.borderless], backing: .buffered, defer: false)
        window.contentView = view
        RunLoop.current.run(until: Date(timeIntervalSinceNow: 0.3))
        view.layoutSubtreeIfNeeded()
        XCTAssertFalse(window.isVisible)
        let image = try XCTUnwrap(view.bitmapImageRepForCachingDisplay(in: view.bounds))
        view.cacheDisplay(in: view.bounds, to: image)
        XCTAssertGreaterThanOrEqual(image.pixelsWide, Int(view.bounds.width))
        if let directory = ProcessInfo.processInfo.environment["VPNMGR_NATIVE_FIXTURE_DIR"] {
            try XCTUnwrap(image.representation(using: .png, properties: [:])).write(to: URL(fileURLWithPath: directory).appendingPathComponent("native-workspace.png"))
        }
        window.contentView = nil
    }
}
