import SwiftUI
import AppKit

@main struct VPNManagerApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    @StateObject private var model = AppModel(automaticRefresh: false)
    var body: some Scene {
        Window("VPN 管理网关", id: "main") {
            WebWorkspaceView().environmentObject(model)
                .background(WindowObserver(model: model, delegate: delegate))
                .frame(minWidth: 920, minHeight: 620)
                .onAppear { delegate.model = model; model.launch() }
        }.defaultSize(width: 1100, height: 760)
            .commands { WorkspaceCommands(model: model) }
        MenuBarExtra("VPN 管理网关", systemImage: "network") {
            MenuContent().environmentObject(model)
        }
    }
}
struct MenuContent: View {
    @EnvironmentObject var model: AppModel
    @Environment(\.openWindow) var openWindow
    var body: some View {
        GatewayStatusSummary().task { await model.refresh() }
        ForEach(model.channels) { ch in Text("\(ch.name) · \(ch.statusLabel(runtime: model.system?.runtime))") }
        Divider()
        Button("刷新状态") { Task { await model.refresh() } }.disabled(!model.ready)
        Button("显示管理窗口") { openWindow(id: "main"); NSApp.activate(ignoringOtherApps: true) }
        Button("断开并退出") { NSApp.terminate(nil) }.keyboardShortcut("q")
    }
}
@MainActor final class AppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate {
    weak var model: AppModel?
    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply { model?.quit() ?? .terminateNow }
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { false }
    func windowShouldClose(_ sender: NSWindow) -> Bool { sender.orderOut(nil); model?.visible = false; return false }
    func windowDidChangeOcclusionState(_ notification: Notification) {
        guard let window = notification.object as? NSWindow else { return }
        model?.visible = window.isVisible && window.occlusionState.contains(.visible) && !NSApp.isHidden
    }
    func applicationDidHide(_ notification: Notification) { model?.visible = false }
    func applicationDidUnhide(_ notification: Notification) { model?.visible = true }
}
struct WindowObserver: NSViewRepresentable {
    let model: AppModel; let delegate: AppDelegate
    func makeNSView(context: Context) -> NSView { NSView() }
    func updateNSView(_ view: NSView, context: Context) { DispatchQueue.main.async { view.window?.delegate = delegate } }
}

enum Page: String, CaseIterable, Identifiable {
    case channels = "通道", rules = "分流规则", monitor = "流量监控", settings = "设置"
    var id: String { rawValue }
    var icon: String { switch self { case .channels: "network"; case .rules: "line.3.horizontal.decrease"; case .monitor: "waveform.path.ecg"; case .settings: "gearshape" } }
}
