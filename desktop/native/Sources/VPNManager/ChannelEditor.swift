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
    @State private var versions: [String] = []
    @State private var filenames: [String: String] = [:]
    private var adapter: Adapter? { model.adapters.first { $0.key == adapterID } }
    private var key: String { channel?.id ?? "__create" }
    private var busy: Bool { model.busy.contains(key) }
    private var complete: Bool {
        !name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && (channel != nil || adapter != nil) && (channel != nil || (adapter?.inputs.filter { $0.required == true }.allSatisfy { !(fields[$0.key] ?? "").trimmingCharacters(in: .whitespacesAndNewlines).isEmpty } ?? true))
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
                        if versions.isEmpty { TextField("客户端版本", text: $version) }
                        else { Picker("客户端版本", selection: $version) { ForEach(versions, id: \.self) { Text($0).tag($0) } } }
                    }
                    if channel != nil {
                        TextField("网关地址", text: binding("server"))
                        TextField("账号", text: binding("username"))
                        SecureField("新密码（留空保持原密码）", text: binding("password"))
                        if channel?.login_method == "byo" { Text("自装客户端的连接参数请在登录窗口中修改。").foregroundStyle(.secondary) }
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
            }.formStyle(.grouped)
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
        }.padding(24).frame(width: 540, height: 580)
            .task {
                if let channel {
                    name = channel.name; adapterID = channel.vpn_type; method = channel.login_method; version = channel.ec_ver ?? ""; probe = channel.probe_url
                    fields = ["server": channel.server, "username": channel.username]
                } else { method = adapter?.login_modes.first ?? "gui" }
                await loadVersions()
            }
            .onChange(of: adapterID) { _, _ in if channel == nil { fields = [:]; method = adapter?.login_modes.first ?? "gui" }; Task { await loadVersions() } }
    }
    private func chooseFile(_ field: InputField) {
        let panel = NSOpenPanel(); panel.canChooseDirectories = false; panel.allowsMultipleSelection = false
        guard panel.runModal() == .OK, let url = panel.url else { return }
        do {
            let data = try Data(contentsOf: url)
            guard data.count <= 1024 * 1024, let text = String(data: data, encoding: .utf8) else { throw APIError(message: "请选择小于 1 MB 的文本配置文件。") }
            fields[field.key] = text; filenames[field.key] = url.lastPathComponent
        } catch { model.error = error.localizedDescription }
    }
    private func binding(_ key: String) -> Binding<String> { Binding(get: { fields[key] ?? "" }, set: { fields[key] = $0 }) }
    private func loadVersions() async {
        guard adapter?.versioned == true, let api = model.api else { versions = []; return }
        do {
            let bytes = try await api.data("/api/vpn-types/\(adapterID)/versions")
            let data = try JSONSerialization.jsonObject(with: bytes) as? [String: Any]
            versions = (data?["versions"] as? [[String: Any]] ?? []).compactMap { $0["tag"] as? String }
            if !version.isEmpty, !versions.contains(version) { versions.insert(version, at: 0) }
        } catch { versions = [] }
    }
    private func save() async {
        var body: [String: Any] = ["name": name, "probe_url": probe]
        if let channel {
            for key in ["server", "username", "password"] {
                let value = fields[key] ?? ""
                if key == "password" { if !value.isEmpty { body[key] = value } }
                else if channel.login_method != "byo" { body[key] = value }
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
