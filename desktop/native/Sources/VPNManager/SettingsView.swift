import SwiftUI
import UniformTypeIdentifiers

struct SettingsView: View {
    @EnvironmentObject var model: AppModel
    @State private var category = SettingsCategory.connection
    @State private var inspecting: Inspection?
    @StateObject private var entryRequest = PageRequest()
    @StateObject private var backupRequest = PageRequest()
    @State private var importing = false
    @State private var selectedBackup: SelectedBackup?
    @State private var systemProxy = false
    @State private var tun = false
    @State private var helperUpdateRequired = false
    @State private var entryLoaded = false
    @State private var foreignProxy = false
    @State private var confirmProxy = false
    @State private var helperInstalled = false
    @State private var helperResources = false
    @State private var helperVersion: String?
    @State private var helperAction: String?
    private var isolated: Bool { model.system?.host_integrations_available == false }
    private var hostAvailable: Bool { model.system?.host_integrations_available == true }
    init(initialCategory: SettingsCategory = .connection) { _category = State(initialValue: initialCategory) }
    var body: some View {
        VStack(spacing: 0) {
            Picker("设置分类", selection: $category) {
                ForEach(SettingsCategory.allCases) { Text($0.rawValue).tag($0) }
            }.pickerStyle(.segmented).labelsHidden().frame(maxWidth: 540)
                .padding(.horizontal, 24).padding(.vertical, 18)
            Divider()
            Form {
                switch category {
                case .connection: connectionSettings
                case .backup: backupSettings
                case .maintenance: maintenanceSettings
                }
            }.formStyle(.grouped)
        }.task(id: model.system?.host_integrations_available) { entryRequest.activate(); backupRequest.activate(); await loadEntry() }
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
    @ViewBuilder private var connectionSettings: some View {
        Section("运行环境") {
            RuntimeStatusView()
            if model.system?.self_heal_enabled != nil {
                Toggle(isOn: Binding(get: { model.system?.self_heal_enabled == true }, set: { value in Task { await model.perform("/api/system/self-heal", key: "__self-heal", body: ["enabled": value], success: value ? "自动修复已开启" : "自动修复已暂停") } })) {
                    settingLabel("自动修复连接", detail: "连接异常时尝试恢复；关闭后仍保留基础网络防护。")
                }.disabled(model.busy.contains("__self-heal"))
            }
        }
        Section("分流入口") {
            Toggle(isOn: Binding(get: { model.system?.routing_off != true }, set: { enabled in Task { await model.perform("/api/routing", key: "__routing", body: ["off": !enabled], success: "分流设置已保存") } })) {
                settingLabel("启用分流", detail: "按分流规则将请求送往对应通道。")
            }.disabled(model.system?.routing_off == nil || model.busy.contains("__routing"))
            Toggle(isOn: Binding(get: { systemProxy }, set: { value in if value && foreignProxy { confirmProxy = true } else { setProxy(value) } })) {
                settingLabel("系统自动代理", detail: "供支持系统代理的应用使用。")
            }.disabled(!entryLoaded || entryRequest.loading || !hostAvailable || model.busy.contains("__entry"))
            if foreignProxy { Text("当前自动代理由其他应用设置，启用后将替换。") .font(.caption).foregroundStyle(.orange) }
            Toggle(isOn: Binding(get: { tun }, set: { value in Task { if await model.perform("/api/entry/tun", key: "__entry", body: ["enable": value]) { await loadEntry() } } })) {
                settingLabel("TUN 接管", detail: "接入不使用系统代理的应用，需要安装助手。")
            }.disabled(!entryLoaded || entryRequest.loading || !helperInstalled || !hostAvailable || model.busy.contains("__entry"))
            if hostAvailable {
                LabeledContent {
                    HStack(spacing: 10) {
                        Text(!entryLoaded ? "待确认" : helperInstalled ? "已安装\(helperVersion.map { " · " + $0 } ?? "")" : "未安装").foregroundStyle(.secondary)
                        Menu("管理…") {
                            Button(helperInstalled ? "更新助手…" : "安装助手…") { helperAction = "install" }.disabled(!helperResources)
                            if helperInstalled { Button("卸载助手…", role: .destructive) { helperAction = "uninstall" } }
                            Divider()
                            Button("刷新接入状态") { Task { await loadEntry() } }
                        }.fixedSize().disabled(!entryLoaded || entryRequest.loading || model.busy.contains("__entry"))
                    }
                } label: { Text("TUN 助手") }
                if entryLoaded && helperUpdateRequired {
                    Text("助手版本需要更新。请在“管理”中更新助手，再重试连接。").font(.callout).foregroundStyle(.orange)
                }
                if entryLoaded && !helperResources { Text("当前应用缺少助手安装资源，请使用完整发行版本。").font(.caption).foregroundStyle(.secondary) }
            }
            if isolated { Text("隔离开发版的宿主代理与 TUN 操作已禁用。").font(.caption).foregroundStyle(.secondary) }
            if entryRequest.loading { ProgressView("正在核对接入状态…") }
            if let error = entryRequest.error {
                HStack {
                    Text("接入状态暂不可用：\(error)").font(.caption).foregroundStyle(.red)
                    Spacer()
                    Button("重试") { Task { await loadEntry() } }
                }
            }
        }
    }
    @ViewBuilder private var backupSettings: some View {
        Section {
            LabeledContent {
                Button(backupRequest.loading ? "正在准备…" : "导出备份…") { Task { await exportBackup() } }.disabled(backupRequest.loading)
            } label: { settingLabel("备份当前配置", detail: "保存通道、分流规则和自动登录凭据。") }
            if let error = backupRequest.error { Text("导出失败：\(error)").foregroundStyle(.red) }
            LabeledContent {
                Button("选择备份…") { importing = true }
            } label: { settingLabel("导入配置", detail: "先核对备份内容，再确认导入。") }
        } header: { Text("配置备份") } footer: {
            Text("备份可能包含自动登录凭据，请妥善保存。交互登录密码与登录备注不导出。")
        }
        Section("升级与恢复") {
            LabeledContent {
                Button("打开…") { model.upgradePresented = true }
            } label: { settingLabel("迁移与回退", detail: "检查旧版配置，查看升级记录或恢复配置。") }
        }
    }
    @ViewBuilder private var maintenanceSettings: some View {
        Section("检查与诊断") {
            inspectionRow("环境检查", detail: "检查运行所需组件与连接条件。", path: "/api/preflight")
            inspectionRow("运行诊断", detail: "核对网络、分流入口和防护状态。", path: "/api/diag")
            inspectionRow("运行事件", detail: "查看连接、修复与退出过程。", path: "/api/events")
        }
        Section("资源与集成") {
            inspectionRow("镜像下载源", detail: "管理客户端镜像的下载来源。", path: "/api/mirrors")
            inspectionRow("镜像与资源", detail: "下载、导入或清理客户端镜像。", path: "/api/images")
            inspectionRow("容器清单", detail: "查看运行实例及清理状态。", path: "/api/containers")
            inspectionRow("Clash 配置片段", detail: "查看用于其他代理客户端的接入配置。", path: "/api/clash-snippet")
        }
        Section {
            Text("关闭窗口仅隐藏；从应用菜单选择“断开并退出”会结束本次连接。").font(.caption).foregroundStyle(.secondary)
        }
    }
    private func settingLabel(_ title: String, detail: String) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title)
            Text(detail).font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
        }.padding(.vertical, 3).accessibilityElement(children: .combine)
    }
    private func inspectionRow(_ title: String, detail: String, path: String) -> some View {
        LabeledContent {
            Button("打开…") { inspecting = Inspection(path: path, title: title) }.accessibilityLabel("打开\(title)")
        } label: { settingLabel(title, detail: detail) }
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
            helperUpdateRequired = status.needsUpdate
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
    let expected_version: String?
    var needsUpdate: Bool {
        guard installed, let expected_version, let version = helper?.version else { return false }
        return version != expected_version
    }
    struct Helper: Decodable { let version: String? }
}
struct ProxyEntryStatus: Decodable { let enabled: Bool; let is_ours: Bool }

enum SettingsCategory: String, CaseIterable, Identifiable {
    case connection = "连接与分流", backup = "备份与恢复", maintenance = "高级维护"
    var id: String { rawValue }
}
