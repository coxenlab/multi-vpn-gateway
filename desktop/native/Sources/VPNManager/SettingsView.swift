import SwiftUI
import UniformTypeIdentifiers

struct SettingsView: View {
    @EnvironmentObject var model: AppModel
    @State private var inspecting: Inspection?
    @State private var exporting = false
    @State private var importing = false
    @State private var pendingImport: [String: Any]?
    @State private var confirmImport = false
    @State private var systemProxy = false
    @State private var tun = false
    @State private var entryLoaded = false
    @State private var foreignProxy = false
    @State private var confirmProxy = false
    private var isolated: Bool { ProcessInfo.processInfo.environment["VPNMGR_DEV_MODE"] == "1" }
    var body: some View {
        Form {
            Section("运行环境") {
                LabeledContent("状态", value: model.system?.runtime?.label ?? "待确认")
                if let detail = model.system?.runtime?.detail, !detail.isEmpty { Text(detail).foregroundStyle(.secondary) }
                Button("连接运行环境") { Task { await model.perform("/api/runtime/start", key: "__runtime", success: "运行环境已连接") } }.disabled(model.busy.contains("__runtime"))
                Button("检查环境") { inspecting = Inspection(path: "/api/preflight", title: "环境检查") }
                if model.system?.self_heal_enabled != nil {
                    Toggle("自动修复连接", isOn: Binding(get: { model.system?.self_heal_enabled == true }, set: { value in Task { await model.perform("/api/system/self-heal", key: "__self-heal", body: ["enabled": value], success: value ? "自动修复已开启" : "自动修复已暂停") } })).disabled(model.busy.contains("__self-heal"))
                    Text("基础网络防护持续生效，不受此开关影响。").font(.caption).foregroundStyle(.secondary)
                }
                Button("查看运行诊断") { inspecting = Inspection(path: "/api/diag", title: "运行诊断") }
            }
            Section("分流入口") {
                Toggle("启用分流", isOn: Binding(get: { model.system?.routing_off != true }, set: { enabled in Task { await model.perform("/api/routing", key: "__routing", body: ["off": !enabled], success: "分流设置已保存") } })).disabled(model.system?.routing_off == nil || model.busy.contains("__routing"))
                Toggle("系统自动代理", isOn: Binding(get: { systemProxy }, set: { value in if value && foreignProxy { confirmProxy = true } else { setProxy(value) } })).disabled(!entryLoaded || isolated || model.busy.contains("__entry"))
                if foreignProxy { Text("当前自动代理由其他应用设置。启用此入口会替换当前设置。").font(.caption).foregroundStyle(.secondary) }
                Toggle("TUN 接管", isOn: Binding(get: { tun }, set: { value in Task { if await model.perform("/api/entry/tun", key: "__entry", body: ["enable": value]) { await loadEntry() } } })).disabled(!entryLoaded || isolated || model.busy.contains("__entry"))
                if isolated { Text("隔离开发版的宿主代理与 TUN 操作已禁用。").font(.caption).foregroundStyle(.secondary) }
                Button("Clash 配置片段") { inspecting = Inspection(path: "/api/clash-snippet", title: "Clash 配置片段") }
                Button("重新同步已保存规则") { Task { await model.perform("/api/config/retry", key: "__sync", success: "规则同步完成") } }.disabled(model.busy.contains("__sync"))
            }
            Section("配置与备份") {
                Button("导出配置备份…") { Task { await exportBackup() } }
                Button("导入配置备份…") { importing = true }
                Text("备份可能包含自动登录凭据，请妥善保存。交互登录密码与登录备注不导出。").font(.caption).foregroundStyle(.secondary)
            }
            Section("维护") {
                Button("运行事件") { inspecting = Inspection(path: "/api/events", title: "运行事件") }
                Button("镜像下载源") { inspecting = Inspection(path: "/api/mirrors", title: "镜像下载源") }
                Button("镜像与资源") { inspecting = Inspection(path: "/api/images", title: "镜像与资源") }
                Button("容器清单") { inspecting = Inspection(path: "/api/containers", title: "容器清单") }
                Text("关闭窗口仅隐藏；从菜单选择“断开并退出”才会结束本次连接。").font(.caption).foregroundStyle(.secondary)
            }
        }.formStyle(.grouped).task { await loadEntry() }
            .sheet(item: $inspecting) { item in VStack {
                if item.path == "/api/preflight" { EnvironmentView() }
                else if item.path == "/api/images" { ImagesView() }
                else if item.path == "/api/mirrors" { MirrorsView() }
                else { TextEndpointView(path: item.path, title: item.title) }
                Button("关闭") { inspecting = nil }.keyboardShortcut(.cancelAction).padding() }.frame(width: 680, height: 500) }
            .confirmationDialog("替换当前自动代理？", isPresented: $confirmProxy, titleVisibility: .visible) {
                Button("使用本应用的自动代理") { setProxy(true) }
            } message: { Text("这会改变当前网络服务的自动代理入口。现有代理 URL 会被替换。") }
            .fileImporter(isPresented: $importing, allowedContentTypes: [.json]) { result in
                do {
                    let url = try result.get(); let scoped = url.startAccessingSecurityScopedResource(); defer { if scoped { url.stopAccessingSecurityScopedResource() } }
                    guard let body = try JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any], body["kind"] as? String == "vpnmgr-export" else { throw APIError(message: "不是有效的配置备份") }
                    pendingImport = body; confirmImport = true
                } catch { model.error = error.localizedDescription }
            }
            .confirmationDialog("导入配置备份？", isPresented: $confirmImport, titleVisibility: .visible) {
                Button("导入通道与规则") { Task { if let body = pendingImport { await model.perform("/api/config/import", key: "__import", body: body, success: "配置已导入，请按需启动通道") }; pendingImport = nil } }
            } message: { Text("将添加备份中的通道与规则，同名通道跳过。导入不会启动通道。") }
    }
    private func setProxy(_ enabled: Bool) { Task { if await model.perform("/api/entry/system-proxy", key: "__entry", body: ["enable": enabled]) { await loadEntry() } } }
    private func loadEntry() async {
        guard let api = model.api, !isolated else { return }
        do {
            let data = try await api.data("/api/entry/tun")
            let status = try JSONSerialization.jsonObject(with: data) as? [String: Any]; tun = status?["enabled"] as? Bool ?? false
            let proxyData = try await api.data("/api/entry/system-proxy")
            let proxy = try JSONSerialization.jsonObject(with: proxyData) as? [String: Any]; systemProxy = proxy?["enabled"] as? Bool == true && proxy?["is_ours"] as? Bool == true
            foreignProxy = proxy?["enabled"] as? Bool == true && proxy?["is_ours"] as? Bool != true
            entryLoaded = true
        } catch { entryLoaded = false }
    }
    private func exportBackup() async {
        guard let api = model.api else { return }
        do {
            let bytes = try await api.data("/api/config/export")
            let panel = NSSavePanel(); panel.allowedContentTypes = [.json]; panel.nameFieldStringValue = "vpnmgr-backup.json"
            guard panel.runModal() == .OK, let url = panel.url else { return }
            try bytes.write(to: url, options: .atomic)
            try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: url.path)
            model.message = "配置备份已保存"
        } catch { model.error = "导出失败：\(error.localizedDescription)" }
    }
}
struct Inspection: Identifiable { let path: String; let title: String; var id: String { path } }
