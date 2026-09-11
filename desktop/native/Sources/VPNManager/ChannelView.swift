import SwiftUI
import WebKit

struct ChannelView: View {
    @EnvironmentObject var model: AppModel
    let channel: Channel
    @State private var editing = false
    @State private var deleting = false
    @State private var tab = "概览"
    @State private var loginURL: URL?
    private var busy: Bool { model.busy.contains(channel.id) }
    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            HStack(alignment: .top) {
                VStack(alignment: .leading, spacing: 5) { Text(channel.name).font(.title2.bold()); Text(channel.statusLabel(runtime: model.system?.runtime)).foregroundStyle(.secondary) }
                Spacer()
                if busy { ProgressView().controlSize(.small) }
                Button(channel.replacement?.phase == "queued" ? "应用并启动" : channel.needsStart ? "启动" : "停止") {
                    action(channel.needsStart ? "start" : "stop")
                }.buttonStyle(.borderedProminent).disabled(busy)
                if channel.needsStart && channel.configured_status != "stopped" { Button("停用") { action("stop") }.disabled(busy) }
                Menu {
                    Button("编辑连接信息") { editing = true }
                    if channel.login_method == "byo" { Button("上传客户端安装包…") { uploadInstaller() }.disabled(channel.needsStart || model.system?.runtime?.ready != true) }
                    Button("检测连通") { action("status", method: "GET", message: "检测完成") }.disabled(channel.needsStart)
                    Button(channel.routing_enabled ? "暂停参与分流" : "恢复参与分流") { Task { await model.perform("/api/channels/\(channel.id)", key: channel.id, method: "PATCH", body: ["routing_enabled": !channel.routing_enabled], success: "已保存分流设置") } }
                    Divider(); Button("删除通道", role: .destructive) { deleting = true }
                } label: { Image(systemName: "ellipsis.circle") }.disabled(busy)
            }
            if channel.stop_pending == true { Label("已保存停用，环境恢复后确认停止。", systemImage: "clock").font(.callout).foregroundStyle(.secondary) }
            if let pending = channel.replacement {
                HStack {
                    Text(pending.phase == "queued" ? "连接设置已保存，启动后应用" : pending.phase == "awaiting_login" ? "新设置等待登录验证" : "上次操作仍在处理中")
                    Spacer()
                    if pending.can_restore { Button(pending.phase == "queued" ? "撤销修改" : "恢复上一次设置") { action("restore", message: "已恢复设置") }.disabled(busy || (channel.stop_pending == true && pending.phase != "queued")) }
                }.font(.callout).padding(12).background(.quaternary, in: RoundedRectangle(cornerRadius: 8))
            }
            Picker("通道内容", selection: $tab) {
                Text("概览").tag("概览")
                if channel.login_method != "headless" { Text("登录").tag("登录") }
                Text("分流规则").tag("分流规则"); Text("日志").tag("日志")
            }.pickerStyle(.segmented)
            switch tab {
            case "登录":
                if channel.needsStart { ContentUnavailableView("请先启动通道", systemImage: "power") }
                else if model.visible, let loginURL { LoginWebView(url: loginURL).id(channel.container_id).frame(maxWidth: .infinity, maxHeight: .infinity) }
                else { ContentUnavailableView("登录窗口已暂停", systemImage: "pause.circle") }
            case "分流规则": RulesView(channelID: channel.id)
            case "日志": TextEndpointView(path: "/api/channels/\(channel.id)/logs", title: "通道日志")
            default:
                Form {
                    Section("连接信息") {
                        LabeledContent("类型", value: model.adapters.first(where: { $0.key == channel.vpn_type })?.label ?? channel.vpn_type)
                        LabeledContent("网关", value: channel.server.isEmpty ? "未填写" : channel.server).textSelection(.enabled)
                        LabeledContent("账号", value: channel.username.isEmpty ? "未填写" : channel.username)
                        LabeledContent("验证地址", value: channel.probe_url.isEmpty ? "未填写" : channel.probe_url)
                        Button("编辑连接信息") { editing = true }
                    }
                    Section("连接状态") {
                        LabeledContent("状态", value: channel.statusLabel(runtime: model.system?.runtime))
                        LabeledContent("最近延迟", value: channel.status == "logged_in" ? channel.latency_ms.map { "\($0) ms" } ?? "待检测" : "—")
                        Text("是否连通以内网检测结果为准。").font(.caption).foregroundStyle(.secondary)
                        Button("检测连通") { action("status", method: "GET", message: "检测完成") }.disabled(channel.needsStart || busy)
                    }
                    ChannelNoteView(channelID: channel.id, draft: model.noteDraft(for: channel.id))
                }.formStyle(.grouped)
            }
        }.padding(20).frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
            .task(id: channel.id) {
                if let api = model.api {
                    loginURL = await api.url("/native-login.html?id=\(channel.id)")
                }
            }
            .task(id: channel.id) {
                while !Task.isCancelled {
                    if model.visible, model.system?.runtime?.ready == true, let current = model.channels.first(where: { $0.id == channel.id }), !current.needsStart, let api = model.api {
                        _ = try? await api.data("/api/channels/\(channel.id)/health")
                        await model.refresh()
                    }
                    do { try await Task.sleep(for: .seconds(5)) } catch { return }
                }
            }
            .sheet(isPresented: $editing) { ChannelEditor(channel: channel) }
            .confirmationDialog("删除“\(channel.name)”？", isPresented: $deleting, titleVisibility: .visible) {
                Button(needsRuntimeForDelete ? "启动环境并删除" : "删除通道", role: .destructive) {
                    Task { await model.perform("/api/channels/\(channel.id)\(needsRuntimeForDelete ? "?prepare_runtime=true" : "")", key: channel.id, method: "DELETE", success: "通道已删除") }
                }
            } message: { Text(needsRuntimeForDelete ? "需要启动运行环境来清理这条通道的实例与关联数据。此操作无法撤销。" : "将删除这条通道及其关联数据，此操作无法撤销。") }
    }
    private func uploadInstaller() {
        let panel = NSOpenPanel(); panel.canChooseDirectories = false; panel.allowsMultipleSelection = false
        guard panel.runModal() == .OK, let file = panel.url, let api = model.api else { return }
        Task {
            model.busy.insert(channel.id); defer { model.busy.remove(channel.id) }
            do { try await api.upload("/api/channels/\(channel.id)/upload", file: file); model.message = "安装包已上传，请在登录桌面中继续安装。" }
            catch { model.error = error.localizedDescription }
        }
    }
    private var needsRuntimeForDelete: Bool { model.system?.runtime?.ready != true && (channel.container_id != nil || channel.replacement != nil) }
    private func action(_ name: String, method: String = "POST", message: String = "操作已完成") {
        Task { await model.perform("/api/channels/\(channel.id)/\(name)", key: channel.id, method: method, success: message) }
    }
}

struct LoginWebView: NSViewRepresentable {
    let url: URL
    func makeNSView(context: Context) -> WKWebView {
        let config = WKWebViewConfiguration(); config.websiteDataStore = .nonPersistent()
        let view = WKWebView(frame: .zero, configuration: config); view.load(URLRequest(url: url)); return view
    }
    func updateNSView(_ view: WKWebView, context: Context) { if view.url != url { view.load(URLRequest(url: url)) } }
    static func dismantleNSView(_ view: WKWebView, coordinator: ()) {
        view.evaluateJavaScript("window.dispatchEvent(new Event('pagehide'))", completionHandler: nil)
        view.stopLoading(); view.loadHTMLString("", baseURL: nil)
    }
}
