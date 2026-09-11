import SwiftUI
import AppKit

@main struct VPNManagerApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    @StateObject private var model = AppModel()
    var body: some Scene {
        Window("VPN 管理网关", id: "main") {
            WorkspaceView().environmentObject(model)
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
struct WorkspaceView: View {
    @EnvironmentObject var model: AppModel
    @FocusedValue(\.pageRefresh) private var pageRefresh
    @State private var page: Page? = .channels
    @State private var selection: String?
    @State private var creating = false
    @State private var channelSearch = ""
    private var filteredChannels: [Channel] {
        model.channels.filter { channelSearch.isEmpty || $0.name.localizedCaseInsensitiveContains(channelSearch) }
    }
    init(initialSelection: String? = nil, initialPage: Page = .channels) { _selection = State(initialValue: initialSelection); _page = State(initialValue: initialPage) }
    var body: some View {
        NavigationSplitView {
            List(Page.allCases, selection: $page) { item in Label(item.rawValue, systemImage: item.icon).tag(item) }
                .navigationSplitViewColumnWidth(min: 160, ideal: 180, max: 210)
                .safeAreaInset(edge: .bottom) {
                    if model.visible { GatewayStatusSummary().font(.caption).padding() }
                }
        } detail: {
            VStack(spacing: 0) {
                if let error = model.error {
                    HStack(alignment: .top) { Label(error, systemImage: "exclamationmark.triangle"); Spacer(); Button("关闭") { model.error = nil } }
                        .padding().background(.orange.opacity(0.12)).accessibilityElement(children: .contain)
                } else if let message = model.message {
                    HStack { Label(message, systemImage: "info.circle"); Spacer(); Button { model.message = nil } label: { Image(systemName: "xmark") }.buttonStyle(.plain).help("关闭提示").accessibilityLabel("关闭提示") }.font(.callout).padding(.horizontal, 16).padding(.vertical, 8).background(.quaternary)
                }
                if model.ready && model.visible { GatewayStatusView { page = $0 } }
                if !model.ready {
                    ContentUnavailableView {
                        Label(model.quitting ? "正在断开并退出" : model.starting ? "正在准备管理界面" : "本地服务尚未就绪", systemImage: "network")
                    } description: { Text("管理界面就绪后，连接通道时才会准备运行环境。") } actions: {
                        if model.starting || model.quitting { ProgressView() } else { Button("重试") { model.launch() } }
                        if model.canManageUpgrade { Button("升级配置与恢复…") { model.upgradePresented = true }.disabled(model.upgradeSwitching) }
                    }
                } else {
                    switch page ?? .channels {
                    case .channels:
                        HSplitView {
                            VStack(spacing: 0) {
                                HStack {
                                    Text("全部通道").font(.headline)
                                    Text("\(model.channels.count)").foregroundStyle(.secondary).monospacedDigit()
                                    Spacer()
                                    Button { creating = true } label: { Image(systemName: "plus") }
                                        .help("新建通道（⌘N）").accessibilityLabel("新建通道")
                                }.padding(.horizontal, 14).padding(.top, 16).padding(.bottom, 10)
                                TextField("搜索通道", text: $channelSearch).textFieldStyle(.roundedBorder)
                                    .padding(.horizontal, 12).padding(.bottom, 12)
                                Divider()
                                List(selection: $selection) {
                                    ForEach(filteredChannels) { ch in
                                        VStack(alignment: .leading, spacing: 5) {
                                            Text(ch.name).font(.headline).lineLimit(2).help(ch.name)
                                            Text(ch.statusLabel(runtime: model.system?.runtime)).font(.caption).foregroundStyle(.secondary)
                                        }.padding(.vertical, 6).tag(ch.id)
                                    }
                                }.overlay {
                                    if filteredChannels.isEmpty {
                                        Text(model.channels.isEmpty ? "还没有通道" : "没有匹配的通道").foregroundStyle(.secondary)
                                    }
                                }
                            }.frame(minWidth: 210, idealWidth: 230, maxWidth: 260)
                            if let ch = model.channels.first(where: { $0.id == selection }) { ChannelView(channel: ch).id(ch.id) }
                            else { ContentUnavailableView(model.channels.isEmpty ? "添加第一条通道" : "选择一条通道", systemImage: "network", description: Text("在这里管理连接、登录和分流规则。")) }
                        }
                    case .rules: RulesView()
                    case .monitor: MonitorView()
                    case .settings: SettingsView()
                    }
                }
            }.navigationTitle(page?.rawValue ?? "通道")
                .toolbar {
                    Button {
                        if let pageRefresh { pageRefresh.perform() }
                        else { Task { await model.refresh() } }
                    } label: { Label("刷新", systemImage: "arrow.clockwise") }.disabled(!model.ready || pageRefresh?.enabled == false)
                }
        }.onChange(of: model.createdChannelID) { _, id in if let id { selection = id; channelSearch = ""; page = .channels; model.createdChannelID = nil } }
        .task(id: model.message) {
            guard let message = model.message else { return }
            do { try await Task.sleep(for: .seconds(6)) } catch { return }
            if model.message == message { model.message = nil }
        }
        .sheet(isPresented: $creating) { ChannelEditor(channel: nil) }
        .sheet(isPresented: $model.upgradePresented) {
            VStack {
                UpgradeView()
                Button("关闭") { model.upgradePresented = false }.keyboardShortcut(.cancelAction)
                    .disabled(model.upgradePreparing || model.upgradeSwitching).padding()
            }.frame(width: 780, height: 660).interactiveDismissDisabled(model.upgradePreparing || model.upgradeSwitching)
        }
        .focusedSceneValue(\.workspaceActions, WorkspaceActions(create: {
            guard model.ready, NSApp.modalWindow == nil, NSApp.keyWindow?.sheetParent == nil else { return }
            creating = true
        }, settings: {
            guard model.ready, NSApp.modalWindow == nil, NSApp.keyWindow?.sheetParent == nil else { return }
            page = .settings
        }))
    }
}
