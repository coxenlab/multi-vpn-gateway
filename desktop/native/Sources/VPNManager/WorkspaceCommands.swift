import SwiftUI
import AppKit

struct PageRefreshAction {
    var enabled = true
    let perform: () -> Void
}
struct WorkspaceActions {
    let create: () -> Void
    let settings: () -> Void
}
private struct PageRefreshKey: FocusedValueKey { typealias Value = PageRefreshAction }
private struct WorkspaceActionsKey: FocusedValueKey { typealias Value = WorkspaceActions }
extension FocusedValues {
    var pageRefresh: PageRefreshAction? {
        get { self[PageRefreshKey.self] }
        set { self[PageRefreshKey.self] = newValue }
    }
    var workspaceActions: WorkspaceActions? {
        get { self[WorkspaceActionsKey.self] }
        set { self[WorkspaceActionsKey.self] = newValue }
    }
}
extension View {
    func pageRefresh(enabled: Bool = true, action: @escaping () -> Void) -> some View {
        focusedSceneValue(\.pageRefresh, PageRefreshAction(enabled: enabled, perform: action))
    }
}

struct WorkspaceCommands: Commands {
    @ObservedObject var model: AppModel
    @FocusedValue(\.pageRefresh) private var pageRefresh
    @FocusedValue(\.workspaceActions) private var workspace
    var body: some Commands {
        CommandGroup(replacing: .newItem) {
            Button("新建通道…") { workspace?.create() }.keyboardShortcut("n")
                .disabled(!model.ready || workspace == nil || NSApp.modalWindow != nil || NSApp.keyWindow?.sheetParent != nil)
        }
        CommandGroup(after: .newItem) {
            Button("刷新") {
                if let pageRefresh { pageRefresh.perform() }
                else { Task { await model.refresh() } }
            }.keyboardShortcut("r").disabled(!model.ready || pageRefresh?.enabled == false)
        }
        CommandGroup(replacing: .appSettings) {
            Button("设置…") { workspace?.settings() }.keyboardShortcut(",")
                .disabled(!model.ready || workspace == nil || NSApp.modalWindow != nil || NSApp.keyWindow?.sheetParent != nil)
            Button("升级配置与恢复…") { model.upgradePresented = true }
                .disabled(!model.canManageUpgrade || model.upgradeSwitching)
        }
    }
}
