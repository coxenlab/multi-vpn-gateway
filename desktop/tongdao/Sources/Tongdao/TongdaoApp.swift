import SwiftUI
import AppKit

@main struct TongdaoApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    @StateObject private var model = AppModel()
    var body: some Scene {
        Window("通道管家", id: "main") {
            WebWorkspaceView().environmentObject(model)
                .overlay(alignment: .topLeading) { WindowDragArea().frame(width: 196, height: 50) }
                .ignoresSafeArea()
                .background(WindowObserver(model: model, delegate: delegate))
                .frame(minWidth: 1100, minHeight: 720)
                .onAppear { delegate.model = model; model.launch() }
        }.windowStyle(.hiddenTitleBar)
            .defaultSize(width: 1280, height: 800)
            .commands { WorkspaceCommands(model: model) }
        MenuBarExtra {
            MenuBarPanel().environmentObject(model)
        } label: {
            MenuBarLabel(count: MenuBarPanel.menuBarCount(ready: model.ready, channels: model.channels, stale: model.snapshotStale))
        }.menuBarExtraStyle(.window)
    }
}
@MainActor final class AppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate {
    weak var model: AppModel?
    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply { model?.quit() ?? .terminateNow }
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { false }
    // Closing hides the window unless app-settings.json turns keep_running_on_close off;
    // quitting then runs the complete shutdown through applicationShouldTerminate.
    func windowShouldClose(_ sender: NSWindow) -> Bool {
        guard AppModel.keepRunningOnClose(dataDirectory: model?.dataDirectory) else { NSApp.terminate(nil); return false }
        sender.orderOut(nil); model?.visible = false; return false
    }
    func windowDidChangeOcclusionState(_ notification: Notification) {
        guard let window = notification.object as? NSWindow else { return }
        model?.visible = Self.pageVisible(window)
    }
    func applicationDidHide(_ notification: Notification) { model?.visible = false }
    // A window closed to the menu bar stays ordered out, so unhiding the app keeps its page paused.
    func applicationDidUnhide(_ notification: Notification) { model?.visible = Self.pageVisible(model?.window) }
    private static func pageVisible(_ window: NSWindow?) -> Bool {
        guard let window else { return false }
        return pageVisible(windowVisible: window.isVisible, occlusionVisible: window.occlusionState.contains(.visible), appHidden: NSApp.isHidden)
    }
    nonisolated static func pageVisible(windowVisible: Bool, occlusionVisible: Bool, appHidden: Bool) -> Bool {
        windowVisible && occlusionVisible && !appHidden
    }
}
