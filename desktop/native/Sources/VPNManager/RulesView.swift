import SwiftUI

struct RulesView: View {
    @EnvironmentObject var model: AppModel
    var channelID: String? = nil
    @State private var search = ""
    @State private var adding = false
    @State private var selectedChannel = ""
    @State private var patterns = ""
    @State private var deleting: RuleItem?
    @State private var selected = Set<String>()
    @State private var editing: RuleItem?
    @State private var ruleNote = ""
    @State private var ruleLocked = false
    private var rows: [RuleItem] {
        model.channels.filter { channelID == nil || $0.id == channelID }.flatMap { ch in ch.rules.map { RuleItem(channel: ch, rule: $0) } }
            .filter { search.isEmpty || $0.rule.pattern.localizedCaseInsensitiveContains(search) || $0.channel.name.localizedCaseInsensitiveContains(search) }
    }
    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                TextField("搜索域名、IP 或通道", text: $search).textFieldStyle(.roundedBorder)
                if !selected.isEmpty {
                    Menu("批量操作") {
                        Button("启用所选规则") { batch(true) }
                        Button("停用所选规则") { batch(false) }
                    }.disabled(model.busy.contains("__batch-rules"))
                }
                Button("添加规则") { selectedChannel = channelID ?? model.channels.first?.id ?? ""; adding = true }.disabled(model.channels.isEmpty)
            }.padding([.horizontal, .top])
            if rows.isEmpty { ContentUnavailableView("没有匹配的规则", systemImage: "line.3.horizontal.decrease") }
            else {
                Table(rows, selection: $selected) {
                    TableColumn("启用") { item in
                        Toggle("启用 \(item.rule.pattern)", isOn: Binding(get: { item.rule.enabled != 0 }, set: { next in
                            Task { await model.perform("/api/channels/\(item.channel.id)/rules/\(item.rule.id)", key: "rule-\(item.id)", method: "PATCH", body: ["enabled": next], success: "规则已保存") }
                        })).labelsHidden().disabled(item.rule.locked == 1 || model.busy.contains("rule-\(item.id)"))
                    }.width(48)
                    TableColumn("域名 / IP") { item in
                        Text(item.rule.pattern).font(.system(.body, design: .monospaced)).textSelection(.enabled)
                            .contextMenu { Button("备注与锁定…") { editing = item; ruleNote = item.rule.note ?? ""; ruleLocked = item.rule.locked == 1 } }
                    }
                    TableColumn("通道") { Text($0.channel.name) }
                    TableColumn("状态") { item in Text(item.channel.stop_pending == true ? "停用待确认" : model.system?.routing_off == true || !item.channel.routing_enabled || item.channel.configured_status == "stopped" ? "暂不分流" : item.rule.enabled == 0 ? "未启用" : "已保存").foregroundStyle(.secondary) }
                    TableColumn("") { item in Button(role: .destructive) { deleting = item } label: { Image(systemName: "trash") }.buttonStyle(.borderless).help("删除规则").disabled(item.rule.locked == 1) }.width(36)
                }
            }
            Text("已保存的规则与实际生效状态分开确认。环境休眠时，连接后再同步。").font(.caption).foregroundStyle(.secondary).padding([.horizontal, .bottom])
        }.sheet(isPresented: $adding) {
            VStack(alignment: .leading, spacing: 16) {
                Text("添加分流规则").font(.title2.bold())
                Picker("通道", selection: $selectedChannel) { ForEach(model.channels) { Text($0.name).tag($0.id) } }
                Text("每行一个域名或 IP / CIDR，自动识别类型。").foregroundStyle(.secondary)
                TextEditor(text: $patterns).font(.system(.body, design: .monospaced)).frame(height: 160)
                HStack {
                    Button("取消") { adding = false }.keyboardShortcut(.cancelAction); Spacer()
                    Button("添加") {
                        Task {
                            let lines = patterns.split(whereSeparator: { $0.isWhitespace || $0 == "," }).map(String.init)
                            if await model.perform("/api/channels/\(selectedChannel)/rules", key: "__rules", body: ["patterns": lines], success: "规则已添加") { patterns = ""; adding = false }
                        }
                    }.keyboardShortcut(.defaultAction).disabled(patterns.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || model.busy.contains("__rules"))
                }
            }.padding(24).frame(width: 480)
        }.sheet(item: $editing) { item in
            VStack(alignment: .leading, spacing: 16) {
                Text(item.rule.pattern).font(.headline).textSelection(.enabled)
                TextField("规则备注", text: $ruleNote)
                Toggle("锁定此规则", isOn: $ruleLocked)
                Text("锁定后不会被批量启停或删除。").font(.caption).foregroundStyle(.secondary)
                HStack { Button("取消") { editing = nil }.keyboardShortcut(.cancelAction); Spacer(); Button("保存") {
                    Task { if await model.perform("/api/channels/\(item.channel.id)/rules/\(item.rule.id)", key: "rule-" + item.id, method: "PATCH", body: ["note": ruleNote, "locked": ruleLocked], success: "规则已保存") { editing = nil } }
                }.keyboardShortcut(.defaultAction).disabled(model.busy.contains("rule-" + item.id)) }
            }.padding(24).frame(width: 460)
        }.confirmationDialog("删除这条分流规则？", isPresented: Binding(get: { deleting != nil }, set: { if !$0 { deleting = nil } }), titleVisibility: .visible) {
            if let item = deleting { Button("删除 \(item.rule.pattern)", role: .destructive) { Task { await model.perform("/api/channels/\(item.channel.id)/rules/\(item.rule.id)", key: "rule-\(item.id)", method: "DELETE", success: "规则已删除") }; deleting = nil } }
        }
    }
    private func batch(_ enabled: Bool) {
        let ids = rows.filter { selected.contains($0.id) }.map { $0.rule.id }
        guard !ids.isEmpty else { return }
        Task { if await model.perform("/api/rules", key: "__batch-rules", method: "PATCH", body: ["ids": ids, "enabled": enabled], success: "所选规则已保存") { selected.removeAll() } }
    }
}
struct RuleItem: Identifiable { let channel: Channel; let rule: Rule; var id: String { "\(channel.id)-\(rule.id)" } }
