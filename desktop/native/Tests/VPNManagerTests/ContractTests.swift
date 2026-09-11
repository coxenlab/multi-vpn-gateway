import XCTest
import SwiftUI
@testable import VPNManager

final class ContractTests: XCTestCase {
    func testUpgradePreparationUsesRealCoreAndKeepsSourceUnchanged() throws {
        guard let executable = ProcessInfo.processInfo.environment["VPNMGR_NATIVE_CORE_PATH"], let sourcePath = ProcessInfo.processInfo.environment["VPNMGR_NATIVE_UPGRADE_SOURCE"] else { throw XCTSkip("升级合同验证需要隔离 core 与合成源目录") }
        let source = URL(fileURLWithPath: sourcePath)
        let materialNames = ["vpnmgr.db", "master.key", "infra.json", "config.yaml"]
        let before = try materialNames.map { try Data(contentsOf: source.appendingPathComponent($0)) }
        let parent = FileManager.default.temporaryDirectory.appendingPathComponent("vpnmgr-upgrade-native-" + UUID().uuidString)
        try FileManager.default.createDirectory(at: parent, withIntermediateDirectories: false, attributes: [.posixPermissions: 0o700])
        defer { try? FileManager.default.removeItem(at: parent) }
        let destination = parent.appendingPathComponent("candidate")
        let report = try runUpgradePreparation(executable: URL(fileURLWithPath: executable), environment: ["VPNMGR_DEV_MODE":"1", "VPNMGR_VM_PROFILE":"vpnmgr-native-upgrade-qa"], source: source, destination: destination)
        XCTAssertEqual(report.counts["channels"], 1)
        XCTAssertFalse(report.runtime_verified); XCTAssertFalse(report.ready_to_activate)
        XCTAssertTrue(FileManager.default.fileExists(atPath: destination.appendingPathComponent("upgrade-pending").path))
        XCTAssertEqual(try materialNames.map { try Data(contentsOf: source.appendingPathComponent($0)) }, before)
        let mode = try FileManager.default.attributesOfItem(atPath: destination.appendingPathComponent("master.key").path)[.posixPermissions] as? NSNumber
        XCTAssertEqual(mode?.intValue, 0o600)
        XCTAssertThrowsError(try runUpgradePreparation(executable: URL(fileURLWithPath: executable), environment: [:], source: source, destination: destination))
    }
    func testMaintenanceContractsAndDiagnosticJSON() throws {
        let events = try JSONDecoder().decode(EventFeed.self, from: fixture("native-events.json"))
        XCTAssertTrue(events.enabled)
        XCTAssertTrue(events.events.contains { $0.src == "audit" })
        let containers = try JSONDecoder().decode(ContainerFeed.self, from: fixture("native-containers.json"))
        XCTAssertFalse(containers.docker_available)
        XCTAssertTrue(containers.containers.contains { $0.role == "channel" && $0.stateLabel == "未确认" })
        let value = try JSONDecoder().decode(JSONValue.self, from: Data("{\"id\":9223372036854775807,\"state\":[true,null,\"正常\"]}".utf8))
        XCTAssertTrue(value.formatted.contains("9223372036854775807"))
        XCTAssertTrue(value.formatted.contains("正常"))
        XCTAssertEqual(csvField("a,b\"c"), "\"a,b\"\"c\"")
        XCTAssertEqual(csvField(" =1+1"), "\"' =1+1\"")
    }
    func testRuleOverlapRespectsChannelIntentAndIPFamilies() throws {
        let raw = try JSONSerialization.jsonObject(with: fixture("native-channels.json")) as! [[String: Any]]
        var object = raw[0]; object["id"] = "first"; object["status"] = "running"; object["configured_status"] = "running"
        var first = try JSONDecoder().decode(Channel.self, from: JSONSerialization.data(withJSONObject: object))
        object["id"] = "second"
        var second = try JSONDecoder().decode(Channel.self, from: JSONSerialization.data(withJSONObject: object))
        first.domains = [Rule(id: 1, kind: "domain", pattern: "internal.example", enabled: 1, note: nil, locked: nil)]
        first.ips = [Rule(id: 2, kind: "ip", pattern: "10.0.0.0/8", enabled: 1, note: nil, locked: nil), Rule(id: 3, kind: "ip", pattern: "fd12:1::/48", enabled: 1, note: nil, locked: nil)]
        second.domains = [Rule(id: 4, kind: "domain", pattern: "team.internal.example", enabled: 1, note: nil, locked: nil), Rule(id: 5, kind: "domain", pattern: "notinternal.example", enabled: 1, note: nil, locked: nil)]
        second.ips = [Rule(id: 6, kind: "ip", pattern: "10.2.0.0/16", enabled: 1, note: nil, locked: nil), Rule(id: 7, kind: "ip", pattern: "fd12:1::1/128", enabled: 1, note: nil, locked: nil)]
        XCTAssertEqual(findRuleConflicts(RuleAnalysisInput(channels: [first, second], off: false)).count, 3)
        XCTAssertTrue(findRuleConflicts(RuleAnalysisInput(channels: [first, second], off: true)).isEmpty)
        second.stop_pending = true
        XCTAssertTrue(findRuleConflicts(RuleAnalysisInput(channels: [first, second], off: false)).isEmpty)
        second.stop_pending = false; second.configured_status = "error"
        XCTAssertTrue(findRuleConflicts(RuleAnalysisInput(channels: [first, second], off: false)).isEmpty)
        XCTAssertFalse(try XCTUnwrap(IPRange("0.0.0.0/0")).overlaps(XCTUnwrap(IPRange("::/0"))))
        XCTAssertNil(IPRange("10.0.0.1/33"))
    }
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
