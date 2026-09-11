import SwiftUI

struct ChannelEditor: View {
    @EnvironmentObject var model: AppModel
    @Environment(\.dismiss) var dismiss
    let channel: Channel?
    @State private var step = 0
    @State private var name = ""
    @State private var adapterID = "easyconnect"
    @State private var method = "gui"
    @State private var version = "7.6.3"
    @State private var probe = ""
    @State private var fields: [String: String] = [:]
    @StateObject private var versions = VersionChoices()
    @State private var filenames: [String: String] = [:]
    private var adapter: Adapter? { model.adapters.first { $0.key == adapterID } }
    private var key: String { channel?.id ?? "__create" }
    private var busy: Bool { model.busy.contains(key) }
    private var complete: Bool {
        !name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && (channel != nil || adapter != nil)
        && (adapter?.versioned != true || versions.permits(version, existing: channel?.ec_ver))
        && (adapter?.inputs.filter { $0.required == true && (channel == nil || ["server", "username"].contains($0.key)) }.allSatisfy { !(fields[$0.key] ?? "").trimmingCharacters(in: .whitespacesAndNewlines).isEmpty } ?? (channel != nil))
    }
    init(channel: Channel?) {
        self.channel = channel
        if let channel {
            _name = State(initialValue: channel.name); _adapterID = State(initialValue: channel.vpn_type)
            _method = State(initialValue: channel.login_method); _version = State(initialValue: channel.ec_ver ?? "")
            _probe = State(initialValue: channel.probe_url)
            _fields = State(initialValue: ["server": channel.server, "username": channel.username])
        }
    }
    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text(channel == nil ? "新建通道" : "编辑连接信息").font(.title2.bold())
            if channel == nil { Text(["1 选择 VPN 类型", "2 填写连接信息", "3 确认并连接"][step]).foregroundStyle(.secondary) }
            Form {
                if channel == nil && step == 0 {
                    Picker("VPN 类型", selection: $adapterID) { ForEach(model.adapters) { Text($0.label).tag($0.key) } }
                    Text(adapter?.desc ?? "正在读取支持的 VPN 类型…")
                    if let notice = adapter?.notice { Text(notice).foregroundStyle(.secondary) }
                } else if channel != nil || step == 1 {
                    TextField("通道名称", text: $name)
                    if channel == nil {
                        Picker("登录方式", selection: $method) { ForEach(adapter?.login_modes ?? [], id: \.self) { Text($0 == "headless" ? "账号密码自动登录" : $0 == "byo" ? "自装客户端" : "登录窗口手动登录").tag($0) } }
                    }
                    if adapter?.versioned == true {
                        if versions.loading { ProgressView("正在读取版本…").controlSize(.small) }
                        else if versions.loaded {
                            Picker("客户端版本", selection: $version) {
                                if version.isEmpty { Text("请选择适用版本").tag("") }
                                if let existing = channel?.ec_ver, !existing.isEmpty, !versions.items.contains(where: { $0.tag == existing }) { Text("\(existing)（当前版本）").tag(existing) }
                                ForEach(versions.items) { entry in Text(entry.usable_here ? entry.tag : "\(entry.tag)（本机不可用）").tag(entry.tag).disabled(!entry.usable_here) }
                            }
                            if !versions.items.contains(where: { $0.usable_here }) { Text("暂无适用于本机的可选版本。").foregroundStyle(.secondary) }
                        } else { TextField("客户端版本", text: $version) }
                        if let error = versions.error { HStack { Text(error).foregroundStyle(.secondary); Button("重试") { Task { await loadVersions() } } } }
                    }
                    if channel != nil {
                        if channel?.login_method == "byo" { Text("自装客户端的连接参数请在登录窗口中修改。").foregroundStyle(.secondary) }
                        else {
                            ForEach(adapter?.inputs ?? []) { field in
                                if field.key == "server" || field.key == "username" { TextField(field.label, text: binding(field.key)) }
                                else if field.key == "password" { SecureField("新密码（留空保持原密码）", text: binding("password")) }
                                else if field.type == "file" { Text("\(field.label)：沿用创建时的配置文件。").foregroundStyle(.secondary) }
                            }
                        }
                    } else {
                        ForEach(adapter?.inputs ?? []) { field in
                            if field.type == "file" {
                                if field.key == "package" { Text("通道创建后，可从详情菜单上传客户端安装包。").foregroundStyle(.secondary) }
                                else { HStack { Text(field.label); Spacer(); Button(filenames[field.key] ?? "选择配置文件…") { chooseFile(field) } } }
                            } else if field.secret == true { SecureField(field.label, text: binding(field.key)) }
                            else if ["textarea", "file"].contains(field.type) {
                                VStack(alignment: .leading) { Text(field.label); TextEditor(text: binding(field.key)).frame(height: 90) }
                            } else { TextField(field.label, text: binding(field.key)) }
                        }
                    }
                    TextField("内网验证地址", text: $probe)
                    Text("使用可访问的内网地址验证连接，登录窗口打开不代表已连通。").font(.caption).foregroundStyle(.secondary)
                } else {
                    LabeledContent("通道", value: name)
                    LabeledContent("类型", value: adapter?.label ?? adapterID)
                    LabeledContent("网关", value: fields["server"] ?? "由客户端设置")
                    Text("确认后将准备运行环境并创建通道。首次准备可能需要下载镜像。")
                    Text("密码仅在本机加密保存。").foregroundStyle(.secondary)
                }
            }.formStyle(.grouped).disabled(busy)
            if busy, let detail = model.system?.runtime?.detail, !detail.isEmpty { Text(detail).font(.callout).foregroundStyle(.secondary) }
            if let error = model.error { Text(error).font(.callout).foregroundStyle(.red).textSelection(.enabled) }
            HStack {
                Button("取消") { dismiss() }.keyboardShortcut(.cancelAction).disabled(busy)
                Spacer()
                if channel == nil && step > 0 { Button("上一步") { step -= 1 }.disabled(busy) }
                if channel == nil && step < 2 {
                    Button("继续") { step += 1 }.keyboardShortcut(.defaultAction).disabled(step == 0 ? adapter == nil : !complete)
                } else {
                    Button(busy ? "处理中…" : channel == nil ? "创建并连接" : "保存") { Task { await save() } }.keyboardShortcut(.defaultAction).disabled(busy || !complete)
                }
            }
        }.padding(24).frame(width: 540, height: 580).interactiveDismissDisabled(busy)
            .task(id: adapterID + (adapter == nil ? "-missing" : "-ready")) {
                if channel == nil, adapter?.login_modes.contains(method) != true { method = adapter?.login_modes.first ?? "gui" }
                await loadVersions()
            }
            .onChange(of: adapterID) { _, _ in if channel == nil { fields = [:]; filenames = [:]; version = ""; method = adapter?.login_modes.first ?? "gui" } }
    }
    private func chooseFile(_ field: InputField) {
        let panel = NSOpenPanel(); panel.canChooseDirectories = false; panel.allowsMultipleSelection = false
        guard panel.runModal() == .OK, let url = panel.url else { return }
        do {
            fields[field.key] = try readTextConfiguration(url); filenames[field.key] = url.lastPathComponent
        } catch { model.error = error.localizedDescription }
    }
    private func binding(_ key: String) -> Binding<String> { Binding(get: { fields[key] ?? "" }, set: { fields[key] = $0 }) }
    private func loadVersions() async {
        let requestedAdapter = adapterID, originalVersion = version
        await versions.load(versioned: adapter?.versioned == true) {
            guard let api = model.api else { throw APIError(message: "本地服务尚未就绪") }
            return try await api.get("/api/vpn-types/\(requestedAdapter)/versions", as: VersionFeed.self)
        }
        guard !Task.isCancelled, requestedAdapter == adapterID, originalVersion == version, versions.loaded else { return }
        version = versions.selection(current: version, existing: channel?.ec_ver)
    }
    private func save() async {
        var body: [String: Any] = ["name": name, "probe_url": probe]
        if let channel {
            for key in (adapter?.inputs.map(\.key) ?? []).filter({ ["server", "username", "password"].contains($0) && channel.login_method != "byo" }) {
                let value = fields[key] ?? ""
                if key == "password" { if !value.isEmpty { body[key] = value } }
                else { body[key] = value }
            }
            if adapter?.versioned == true { body["ec_ver"] = version }
        } else {
            body["vpn_type"] = adapterID; body["login_method"] = method == "gui" ? "interactive" : method
            body["config"] = fields; body["server"] = fields["server"] ?? ""; body["username"] = fields["username"] ?? ""; body["password"] = fields["password"] ?? ""
            if adapter?.versioned == true { body["ec_ver"] = version }
        }
        if await model.perform(channel.map { "/api/channels/\($0.id)" } ?? "/api/channels", key: key, method: channel == nil ? "POST" : "PATCH", body: body, success: channel == nil ? "通道已创建，请完成登录并检测连通" : "连接信息已保存") { fields["password"] = nil; dismiss() }
    }
}
