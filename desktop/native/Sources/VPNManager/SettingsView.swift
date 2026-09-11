import SwiftUI
import UniformTypeIdentifiers

struct SettingsView: View {
    @EnvironmentObject var model: AppModel
    @State private var inspecting: Inspection?
    @StateObject private var entryRequest = PageRequest()
    @StateObject private var backupRequest = PageRequest()
    @State private var importing = false
    @State private var selectedBackup: SelectedBackup?
    @State private var systemProxy = false
    @State private var tun = false
    @State private var entryLoaded = false
    @State private var foreignProxy = false
    @State private var confirmProxy = false
    @State private var helperInstalled = false
    @State private var helperResources = false
    @State private var helperVersion: String?
    @State private var helperAction: String?
    private var isolated: Bool { model.system?.host_integrations_available == false }
    private var hostAvailable: Bool { model.system?.host_integrations_available == true }
    var body: some View {
        Form {
            Section("运行环境") {
                RuntimeStatusView()
                Button("检查环境") { inspecting = Inspection(path: "/api/preflight", title: "环境检查") }
                if model.system?.self_heal_enabled != nil {
                    Toggle("自动修复连接", isOn: Binding(get: { model.system?.self_heal_enabled == true }, set: { value in Task { await model.perform("/api/system/self-heal", key: "__self-heal", body: ["enabled": value], success: value ? "自动修复已开启" : "自动修复已暂停") } })).disabled(model.busy.contains("__self-heal"))
                    Text("基础网络防护持续生效，不受此开关影响。").font(.caption).foregroundStyle(.secondary)
                }
                Button("查看运行诊断") { inspecting = Inspection(path: "/api/diag", title: "运行诊断") }
            }
            Section("分流入口") {
                Toggle("启用分流", isOn: Binding(get: { model.system?.routing_off != true }, set: { enabled in Task { await model.perform("/api/routing", key: "__routing", body: ["off": !enabled], success: "分流设置已保存") } })).disabled(model.system?.routing_off == nil || model.busy.contains("__routing"))
                Toggle("系统自动代理", isOn: Binding(get: { systemProxy }, set: { value in if value && foreignProxy { confirmProxy = true } else { setProxy(value) } })).disabled(!entryLoaded || entryRequest.loading || !hostAvailable || model.busy.contains("__entry"))
                if foreignProxy { Text("当前自动代理由其他应用设置。启用此入口会替换当前设置。").font(.caption).foregroundStyle(.secondary) }
                Toggle("TUN 接管", isOn: Binding(get: { tun }, set: { value in Task { if await model.perform("/api/entry/tun", key: "__entry", body: ["enable": value]) { await loadEntry() } } })).disabled(!entryLoaded || entryRequest.loading || !helperInstalled || !hostAvailable || model.busy.contains("__entry"))
                if hostAvailable {
                    LabeledContent("TUN 助手", value: !entryLoaded ? "待确认" : helperInstalled ? "已安装\(helperVersion.map { " · " + $0 } ?? "")" : "未安装")
                    HStack {
                        Button(helperInstalled ? "更新助手…" : "安装助手…") { helperAction = "install" }.disabled(!entryLoaded || entryRequest.loading || !helperResources || model.busy.contains("__entry"))
                        if helperInstalled { Button("卸载助手…", role: .destructive) { helperAction = "uninstall" }.disabled(entryRequest.loading || model.busy.contains("__entry")) }
                        Button("刷新接入状态") { Task { await loadEntry() } }.disabled(entryRequest.loading || model.busy.contains("__entry"))
                    }
                    if entryLoaded && !helperResources { Text("当前应用缺少助手安装资源，请使用完整发行版本。").font(.caption).foregroundStyle(.secondary) }
                }
                if isolated { Text("隔离开发版的宿主代理与 TUN 操作已禁用。").font(.caption).foregroundStyle(.secondary) }
                if entryRequest.loading { ProgressView("正在核对接入状态…") }
                if let error = entryRequest.error { Text("接入状态暂不可用：\(error)").font(.caption).foregroundStyle(.red) }
                Button("Clash 配置片段") { inspecting = Inspection(path: "/api/clash-snippet", title: "Clash 配置片段") }
                ConfigApplicationView()
            }
            Section("配置与备份") {
                Button(backupRequest.loading ? "正在准备备份…" : "导出配置备份…") { Task { await exportBackup() } }.disabled(backupRequest.loading)
                if let error = backupRequest.error { Text("导出失败：\(error)").foregroundStyle(.red) }
                Button("导入配置备份…") { importing = true }
                Button("升级配置与恢复…") { model.upgradePresented = true }
                Text("备份可能包含自动登录凭据，请妥善保存。交互登录密码与登录备注不导出。").font(.caption).foregroundStyle(.secondary)
            }
            Section("维护") {
                Button("运行事件") { inspecting = Inspection(path: "/api/events", title: "运行事件") }
                Button("镜像下载源") { inspecting = Inspection(path: "/api/mirrors", title: "镜像下载源") }
                Button("镜像与资源") { inspecting = Inspection(path: "/api/images", title: "镜像与资源") }
                Button("容器清单") { inspecting = Inspection(path: "/api/containers", title: "容器清单") }
                Text("关闭窗口仅隐藏；从菜单选择“断开并退出”才会结束本次连接。").font(.caption).foregroundStyle(.secondary)
            }
        }.formStyle(.grouped).task(id: model.system?.host_integrations_available) { entryRequest.activate(); backupRequest.activate(); await loadEntry() }
            .onDisappear { entryRequest.suspend(); backupRequest.suspend() }
            .pageRefresh(enabled: !entryRequest.loading) { Task { await model.refresh(); await loadEntry() } }
            .sheet(item: $inspecting) { item in VStack {
                if item.path == "/api/preflight" { EnvironmentView() }
                else if item.path == "/api/images" { ImagesView() }
                else if item.path == "/api/mirrors" { MirrorsView() }
                else if item.path == "/api/events" { EventsView() }
                else if item.path == "/api/containers" { ContainersView() }
                else { TextEndpointView(path: item.path, title: item.title) }
                Button("关闭") { inspecting = nil }.keyboardShortcut(.cancelAction).padding() }.frame(width: 780, height: 620) }
            .confirmationDialog("替换当前自动代理？", isPresented: $confirmProxy, titleVisibility: .visible) {
                Button("使用本应用的自动代理") { setProxy(true) }
            } message: { Text("这会改变当前网络服务的自动代理入口。现有代理 URL 会被替换。") }
            .confirmationDialog(helperAction == "uninstall" ? "卸载 TUN 助手？" : "安装或更新 TUN 助手？", isPresented: Binding(get: { helperAction != nil }, set: { if !$0 { helperAction = nil } }), titleVisibility: .visible) {
                if let action = helperAction {
                    Button(action == "uninstall" ? "卸载助手" : "安装或更新", role: action == "uninstall" ? .destructive : nil) {
                        helperAction = nil
                        Task { await model.perform("/api/entry/tun/\(action)", key: "__entry", success: action == "uninstall" ? "助手已卸载" : "助手已安装或更新"); await loadEntry() }
                    }
                }
            } message: { Text("系统将请求管理员授权。安装更新可能短暂影响 TUN 连接；卸载将结束 TUN 接管。") }
            .fileImporter(isPresented: $importing, allowedContentTypes: [.json]) { result in
                do { selectedBackup = SelectedBackup(url: try result.get()) }
                catch { model.error = error.localizedDescription }
            }
            .sheet(item: $selectedBackup) { selected in BackupImportView(file: selected.url) }
    }
    private func setProxy(_ enabled: Bool) { Task { if await model.perform("/api/entry/system-proxy", key: "__entry", body: ["enable": enabled]) { await loadEntry() } } }
    private func loadEntry() async {
        guard hostAvailable else { entryRequest.cancel(); entryLoaded = false; return }
        await entryRequest.run(model: model, operation: { api in
            async let tun = api.get("/api/entry/tun", as: TunEntryStatus.self)
            async let proxy = api.get("/api/entry/system-proxy", as: ProxyEntryStatus.self)
            return try await (tun, proxy)
        }) { status, proxy in
            tun = status.enabled; helperInstalled = status.installed; helperResources = status.resources == true
            helperVersion = status.helper?.version
            systemProxy = proxy.enabled && proxy.is_ours; foreignProxy = proxy.enabled && !proxy.is_ours
            entryLoaded = true
        }
        if entryRequest.error != nil { entryLoaded = false }
    }
    private func exportBackup() async {
        guard !backupRequest.loading else { return }
        await backupRequest.run(model: model, operation: { api in (api, try await api.data("/api/config/export")) }) { api, bytes in
            let panel = NSSavePanel(); panel.allowedContentTypes = [.json]; panel.nameFieldStringValue = "vpnmgr-backup.json"
            guard panel.runModal() == .OK, model.isCurrent(api), let url = panel.url else { return }
            do {
                try bytes.write(to: url, options: .atomic)
                try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: url.path)
                model.message = "配置备份已保存"
            } catch { model.error = "导出失败：\(error.localizedDescription)" }
        }
    }
}
struct Inspection: Identifiable { let path: String; let title: String; var id: String { path } }
struct TunEntryStatus: Decodable {
    let enabled: Bool; let installed: Bool; let resources: Bool?; let helper: Helper?
    struct Helper: Decodable { let version: String? }
}
struct ProxyEntryStatus: Decodable { let enabled: Bool; let is_ours: Bool }
