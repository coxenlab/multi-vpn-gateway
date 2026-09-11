import XCTest
import SwiftUI
@testable import VPNManager

final class LayoutTests: XCTestCase {
    @MainActor func testMinimumWindowAndMaintenanceLayoutsInBothAppearances() async throws {
        guard let directory = ProcessInfo.processInfo.environment["VPNMGR_NATIVE_FIXTURE_DIR"] else {
            throw XCTSkip("原生布局验证需要隔离核心的 API 夹具")
        }
        let root = URL(fileURLWithPath: directory)
        let channelsData = try Data(contentsOf: root.appendingPathComponent("native-channels.json"))
        let adaptersData = try Data(contentsOf: root.appendingPathComponent("native-adapters.json"))
        let eventsData = try Data(contentsOf: root.appendingPathComponent("native-events.json"))
        let containersData = try Data(contentsOf: root.appendingPathComponent("native-containers.json"))
        let channels = try JSONDecoder().decode([Channel].self, from: channelsData)
        let adapters = try JSONDecoder().decode([Adapter].self, from: adaptersData)
        let transport: LocalAPI.Transport = { request in
            guard request.httpMethod == "GET" else { throw APIError(message: "布局测试禁止写请求") }
            let data: Data
            switch request.url!.path {
            case "/api/events": data = eventsData
            case "/api/containers": data = containersData
            case "/api/mirrors": data = Data(#"[{"id":1,"host":"registry.fixture.invalid","priority":1,"enabled":1}]"#.utf8)
            case "/api/images": data = Data(#"{"images":[{"image":"fixture/image:latest","title":"测试客户端镜像","kind":"pull","present":false}]}"#.utf8)
            case "/api/connections": data = Data(#"{"downloadTotal":1000,"uploadTotal":2000,"connections":[{"id":"fixture","metadata":{"host":"service.fixture.invalid","destinationIP":"192.0.2.1","destinationPort":"443","network":"tcp"},"chains":["DIRECT"],"download":100,"upload":200}]}"#.utf8)
            case "/fixture/logs": data = Data(#"{"lines":["连接检查开始","连接检查完成"]}"#.utf8)
            default:
                guard request.url!.path.hasSuffix("/note") else { throw APIError(message: "布局测试缺少只读响应") }
                data = Data(#"{"note":"测试草稿"}"#.utf8)
            }
            return (data, HTTPURLResponse(url: request.url!, statusCode: 200, httpVersion: nil, headerFields: ["Content-Type":"application/json"])!)
        }
        for (appearance, scheme) in [("light", ColorScheme.light), ("dark", .dark)] {
            let model = AppModel(); model.connect(to: LocalAPI(port: 1, transport: transport))
            model.channels = channels; model.adapters = adapters
            model.system = try JSONDecoder().decode(SystemStatus.self, from: Data(#"{"runtime":{"phase":"ready"},"routing_off":false,"self_heal_enabled":true}"#.utf8))
            defer { model.disconnect() }
            let cases: [(String, CGFloat, CGFloat, AnyView)] = [
                ("channels", 920, 620, AnyView(WorkspaceView(initialSelection: channels[0].id))),
                ("rules", 920, 620, AnyView(WorkspaceView(initialPage: .rules))),
                ("monitor", 920, 620, AnyView(WorkspaceView(initialPage: .monitor))),
                ("settings", 920, 620, AnyView(WorkspaceView(initialPage: .settings))),
                ("images", 780, 620, AnyView(ImagesView())),
                ("containers", 780, 620, AnyView(ContainersView())),
                ("events", 780, 620, AnyView(EventsView())),
                ("mirrors", 780, 620, AnyView(MirrorsView())),
                ("logs", 650, 450, AnyView(TextEndpointView(path: "/fixture/logs", title: "测试日志"))),
            ]
            for (name, width, height, content) in cases {
                let view = NSHostingView(rootView: content.environmentObject(model).environment(\.colorScheme, scheme).background(Color(nsColor: .windowBackgroundColor)))
                view.frame = NSRect(x: 0, y: 0, width: width, height: height)
                let window = NSWindow(contentRect: view.frame, styleMask: [.titled, .closable, .miniaturizable, .resizable], backing: .buffered, defer: false)
                window.appearance = NSAppearance(named: scheme == .dark ? .darkAqua : .aqua)
                window.contentView = view
                defer { window.orderOut(nil); window.contentView = nil }
                try await Task.sleep(for: .milliseconds(180))
                view.layoutSubtreeIfNeeded()
                XCTAssertFalse(window.isVisible, "布局验证不能显示应用窗口")
                let bitmap = try XCTUnwrap(view.bitmapImageRepForCachingDisplay(in: view.bounds))
                view.cacheDisplay(in: view.bounds, to: bitmap)
                try XCTUnwrap(bitmap.representation(using: .png, properties: [:])).write(to: root.appendingPathComponent("native-layout-\(name)-\(appearance).png"))
                window.contentView = nil
            }
        }
    }
}
