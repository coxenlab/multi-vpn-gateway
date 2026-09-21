import SwiftUI
import WebKit

struct ChannelView: View {
    @EnvironmentObject var model: AppModel
    let channel: Channel
    @State private var editing = false
    @State private var deleting = false
    @State private var rebuilding = false
    @State private var tab = "概览"
    @State private var loginURL: URL?
    private var busy: Bool { model.busy.contains(channel.id) }
    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(alignment: .top) {
                VStack(alignment: .leading, spacing: 5) { Text(channel.name).font(.title2.bold()).textSelection(.enabled).fixedSize(horizontal: false, vertical: true); Text(channel.statusLabel(runtime: model.system?.runtime)).font(.callout).foregroundStyle(.secondary) }
                Spacer()
                if busy { ProgressView().controlSize(.small) }
                Button(channel.replacement?.phase == "queued" ? "应用并启动" : channel.needsStart ? "启动" : "停止") {
                    action(channel.needsStart ? "start" : "stop")
                }.buttonStyle(.borderedProminent).disabled(busy)
                Menu {
                    if channel.needsStart && channel.configured_status != "stopped" { Button("停用通道") { action("stop") } }
                    Button("编辑连接信息") { editing = true }
                    if !channel.needsStart && channel.replacement == nil { Button("重建容器…") { rebuilding = true } }
                    if channel.login_method == "byo" { Button("上传客户端安装包…") { uploadInstaller() }.disabled(channel.needsStart || model.system?.runtime?.ready != true) }
                    Button("检测连通") { action("status", method: "GET", message: "检测完成") }.disabled(channel.needsStart)
                    Button(channel.routing_enabled ? "暂停参与分流" : "恢复参与分流") { Task { await model.perform("/api/channels/\(channel.id)", key: channel.id, method: "PATCH", body: ["routing_enabled": !channel.routing_enabled], success: "已保存分流设置") } }
                    Divider(); Button("删除通道", role: .destructive) { deleting = true }
                } label: { Image(systemName: "ellipsis.circle") }.menuIndicator(.hidden).help("更多通道操作").disabled(busy)
            }.padding(20)
            if channel.stop_pending == true { Label("已保存停用，环境恢复后确认停止。", systemImage: "clock").font(.callout).foregroundStyle(.secondary).padding(.horizontal, 20).padding(.bottom, 12) }
            if let pending = channel.replacement {
                HStack {
                    Text(pending.phase == "queued" ? "连接设置已保存，启动后应用" : pending.phase == "awaiting_login" ? "新设置等待登录验证" : "上次操作仍在处理中")
                    Spacer()
                    if pending.can_restore { Button(pending.phase == "queued" ? "撤销修改" : "恢复上一次设置") { action("restore", message: "已恢复设置") }.disabled(busy || (channel.stop_pending == true && pending.phase != "queued")) }
                }.font(.callout).padding(12).background(.quaternary, in: RoundedRectangle(cornerRadius: 8)).padding(.horizontal, 20).padding(.bottom, 12)
            }
            Picker("通道内容", selection: $tab) {
                Text("概览").tag("概览")
                if channel.login_method != "headless" { Text("登录").tag("登录") }
                Text("分流规则").tag("分流规则"); Text("日志").tag("日志")
            }.pickerStyle(.segmented).labelsHidden().frame(maxWidth: 420)
                .padding(.horizontal, 20).padding(.bottom, 16)
            Divider()
            switch tab {
            case "登录":
                if channel.needsStart { ContentUnavailableView("请先启动通道", systemImage: "power") }
                else if model.visible, let loginURL { LoginWebView(url: loginURL).id(channel.container_id).frame(maxWidth: .infinity, maxHeight: .infinity) }
                else { ContentUnavailableView("登录窗口已暂停", systemImage: "pause.circle") }
            case "分流规则": RulesView(channelID: channel.id)
            case "日志": TextEndpointView(path: "/api/channels/\(channel.id)/logs", title: "通道日志")
            default:
                Form {
                    Section {
                        LabeledContent("类型", value: model.adapters.first(where: { $0.key == channel.vpn_type })?.label ?? channel.vpn_type)
                        LabeledContent("网关", value: channel.server.isEmpty ? "未填写" : channel.server).textSelection(.enabled)
                        LabeledContent("账号", value: channel.username.isEmpty ? "未填写" : channel.username)
                        LabeledContent("验证地址", value: channel.probe_url.isEmpty ? "未填写" : channel.probe_url)
                        LabeledContent {
                            HStack(spacing: 12) {
                                Text(channel.status == "logged_in" ? channel.latency_ms.map { "\($0) ms" } ?? "待检测" : "—").foregroundStyle(.secondary)
                                Button("检测连通") { action("status", method: "GET", message: "检测完成") }.disabled(channel.needsStart || busy)
                            }
                        } label: { Text("最近延迟") }.accessibilityElement(children: .contain)
                    } header: {
                        HStack {
                            Text("连接信息")
                            Spacer()
                            Button("编辑…") { editing = true }.buttonStyle(.borderless).accessibilityLabel("编辑连接信息")
                        }
                    } footer: { Text("是否连通以内网检测结果为准。") }
                    ChannelNoteView(channelID: channel.id, draft: model.noteDraft(for: channel.id))
                }.formStyle(.grouped)
            }
        }.frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
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
            .confirmationDialog("重建“\(channel.name)”的容器？", isPresented: $rebuilding, titleVisibility: .visible) {
                Button("重建容器") { rebuild() }
            } message: { Text(rebuildMessage) }
            .confirmationDialog("删除“\(channel.name)”？", isPresented: $deleting, titleVisibility: .visible) {
                Button(needsRuntimeForDelete ? "启动环境并删除" : "删除通道", role: .destructive) {
                    Task { await model.perform("/api/channels/\(channel.id)\(needsRuntimeForDelete ? "?prepare_runtime=true" : "")", key: channel.id, method: "DELETE", success: "通道已删除") }
                }
            } message: { Text(needsRuntimeForDelete ? "需要启动运行环境来清理这条通道的实例与关联数据。此操作无法撤销。" : "将删除这条通道及其关联数据，此操作无法撤销。") }
    }
    private func uploadInstaller() {
        let panel = NSOpenPanel(); panel.canChooseDirectories = false; panel.allowsMultipleSelection = false
        panel.message = "选择不超过 1 GiB 的客户端安装包，上传后在通道桌面中运行。"
        guard panel.runModal() == .OK, let file = panel.url else { return }
        Task { await model.uploadInstaller(file, channelID: channel.id) }
    }
    private var rebuildMessage: String {
        switch channel.login_method {
        case "byo": return "将重启这条通道的容器，已装的客户端保留，需要在登录窗口重新连接。"
        case "headless": return "将用当前设置重新创建容器并自动重连，期间这条通道的流量会中断。"
        default: return "将用当前设置重新创建容器，需要在登录窗口重新登录，期间这条通道的流量会中断。"
        }
    }
    /// 重建容器 = 停止 + 启动(hagb/oss 启动即重建,byo 原地重启);停止未确认时不接着启动。
    private func rebuild() {
        Task {
            guard await model.perform("/api/channels/\(channel.id)/stop", key: channel.id, success: "已停止") else { return }
            guard let current = model.channels.first(where: { $0.id == channel.id }), current.stop_pending != true else { return }
            await model.perform("/api/channels/\(channel.id)/start", key: channel.id, success: channel.login_method == "headless" ? "已重建，正在重连" : "已重建，请重新登录")
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
