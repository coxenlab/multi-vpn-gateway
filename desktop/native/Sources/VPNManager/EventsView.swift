import SwiftUI
import UniformTypeIdentifiers

// Keeps diagnostic JSON structured until the user opens details; numbers retain integer precision.
indirect enum JSONValue: Codable {
    case string(String), integer(Int64), number(Double), bool(Bool), object([String: JSONValue]), array([JSONValue]), null
    init(from decoder: Decoder) throws {
        let value = try decoder.singleValueContainer()
        if value.decodeNil() { self = .null }
        else if let x = try? value.decode(Bool.self) { self = .bool(x) }
        else if let x = try? value.decode(Int64.self) { self = .integer(x) }
        else if let x = try? value.decode(Double.self) { self = .number(x) }
        else if let x = try? value.decode(String.self) { self = .string(x) }
        else if let x = try? value.decode([String: JSONValue].self) { self = .object(x) }
        else { self = .array(try value.decode([JSONValue].self)) }
    }
    func encode(to encoder: Encoder) throws {
        var value = encoder.singleValueContainer()
        switch self {
        case .string(let x): try value.encode(x)
        case .integer(let x): try value.encode(x)
        case .number(let x): try value.encode(x)
        case .bool(let x): try value.encode(x)
        case .object(let x): try value.encode(x)
        case .array(let x): try value.encode(x)
        case .null: try value.encodeNil()
        }
    }
    var formatted: String {
        let encoder = JSONEncoder(); encoder.outputFormatting = [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes]
        return (try? encoder.encode(self)).map { String(decoding: $0, as: UTF8.self) } ?? "无法展示"
    }
}
struct RuntimeEvent: Decodable, Identifiable {
    let seq: UInt64; let ts: String; let level: String; let src: String; let event: String; let msg: String; let detail: JSONValue
    var id: UInt64 { seq }
    var levelLabel: String { ["debug":"调试", "info":"信息", "warn":"警告", "error":"错误"][level] ?? level }
}
struct EventFeed: Decodable { let events: [RuntimeEvent]; let seq: UInt64; let dropped: Int; let enabled: Bool; let retained_days: Int }
struct EventsView: View {
    @EnvironmentObject var model: AppModel
    @State private var events: [RuntimeEvent] = []
    @State private var cursor: UInt64 = 0
    @State private var search = ""
    @State private var level = "all"
    @State private var source = "all"
    @State private var enabled: Bool?
    @State private var paused = false
    @StateObject private var request = PageRequest()
    @State private var dropped = 0
    @State private var error: String?
    @State private var selected: RuntimeEvent?
    @State private var from = Date()
    @State private var to = Date()
    private var filtered: [RuntimeEvent] { events.reversed().filter {
        (level == "all" || $0.level == level) && (source == "all" || $0.src == source) && (search.isEmpty || $0.msg.localizedCaseInsensitiveContains(search) || $0.event.localizedCaseInsensitiveContains(search) || $0.src.localizedCaseInsensitiveContains(search))
    } }
    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text("运行事件").font(.title2.bold()); Spacer()
                if let enabled { Toggle("记录日志", isOn: Binding(get: { enabled }, set: { value in Task { await setEnabled(value) } })).toggleStyle(.switch).disabled(request.loading || model.busy.contains("__events")) }
                if request.loading { ProgressView().controlSize(.small) }
                Button(paused ? "继续刷新" : "暂停刷新") { paused.toggle(); request.cancel(); if !paused { Task { await load() } } }
            }
            HStack {
                TextField("搜索事件", text: $search).textFieldStyle(.roundedBorder)
                Picker("级别", selection: $level) { Text("全部").tag("all"); Text("信息").tag("info"); Text("警告").tag("warn"); Text("错误").tag("error"); Text("调试").tag("debug") }.frame(width: 130)
                Picker("来源", selection: $source) {
                    Text("全部来源").tag("all"); Text("操作记录").tag("audit")
                    ForEach(Array(Set(events.map(\.src) + [source])).filter { !["all", "audit"].contains($0) }.sorted(), id: \.self) { Text($0).tag($0) }
                }.frame(width: 155)
            }
            if let error { Text(error).foregroundStyle(.red) }
            if let error = request.error { Text(error).foregroundStyle(.red) }
            if dropped > 0 { Text("有 \(dropped) 条记录未能写入文件，请结合当前状态判断。").font(.caption).foregroundStyle(.orange) }
            List(filtered) { item in
                Button { selected = item } label: {
                    HStack(alignment: .top, spacing: 10) {
                        Text(item.levelLabel).font(.caption).foregroundStyle(item.level == "error" ? .red : item.level == "warn" ? .orange : .secondary).frame(width: 34)
                        VStack(alignment: .leading, spacing: 4) { Text(item.msg).foregroundStyle(.primary); Text("\(item.ts) · \(item.src)").font(.caption).foregroundStyle(.secondary) }
                        Spacer(); Image(systemName: "chevron.right").foregroundStyle(.tertiary)
                    }.padding(.vertical, 4).contentShape(Rectangle())
                }.buttonStyle(.plain)
            }.overlay { if filtered.isEmpty { ContentUnavailableView("暂无匹配的事件", systemImage: "list.bullet.rectangle") } }
            HStack {
                DatePicker("从", selection: $from, in: ...Date(), displayedComponents: .date)
                DatePicker("至", selection: $to, in: ...Date(), displayedComponents: .date)
                Button("导出此日期范围…") { Task { await export() } }.disabled(Calendar.current.startOfDay(for: from) > Calendar.current.startOfDay(for: to) || model.busy.contains("__events-export"))
            }
            Text("列表保留最近 1000 条；导出最近 14 天内的持久记录，包含该日期范围的所有级别和来源。隐藏窗口后暂停刷新。").font(.caption).foregroundStyle(.secondary)
        }.padding().task {
            request.activate()
            while !Task.isCancelled {
                if model.visible && !paused { await load() }
                do { try await Task.sleep(for: .seconds(5)) } catch { return }
            }
        }.onDisappear { request.suspend() }
            .pageRefresh(enabled: !request.loading) { Task { await load(allowPaused: true) } }
            .onChange(of: model.visible) { _, visible in if !visible { request.cancel() } }
            .sheet(item: $selected) { item in
            VStack(alignment: .leading, spacing: 12) {
                Text(item.msg).font(.headline); Text("\(item.ts) · \(item.src) · \(item.event)").font(.caption).foregroundStyle(.secondary)
                ScrollView { Text(item.detail.formatted).font(.system(.caption, design: .monospaced)).textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading) }
                Button("关闭") { selected = nil }.keyboardShortcut(.cancelAction)
            }.padding(24).frame(width: 620, height: 420)
        }
    }
    private func load(configurationOnly: Bool = false, allowPaused: Bool = false) async {
        await request.run(model: model, operation: { try await $0.get("/api/events?since_seq=\(cursor)&limit=1000", as: EventFeed.self) }) { feed in
            guard model.visible else { return }
            if configurationOnly { enabled = feed.enabled }
            guard !paused || allowPaused else { return }
            if feed.seq < cursor { events = []; cursor = 0; return } // core restarted; fetch its new sequence from zero.
            let known = Set(events.map(\.seq)); events.append(contentsOf: feed.events.filter { !known.contains($0.seq) }); events = Array(events.suffix(1000))
            cursor = feed.seq; enabled = feed.enabled; dropped = feed.dropped; error = nil
        }
    }
    private func setEnabled(_ value: Bool) async {
        if await model.perform("/api/events/enabled", key: "__events", body: ["enabled": value], success: value ? "日志记录已开启" : "日志记录已关闭") { await load(configurationOnly: true) }
    }
    private func export() async {
        guard let api = model.api, model.isCurrent(api), !model.busy.contains("__events-export") else { return }
        model.busy.insert("__events-export"); defer { if model.isCurrent(api) { model.busy.remove("__events-export") } }
        let format = DateFormatter(); format.locale = Locale(identifier: "en_US_POSIX"); format.calendar = Calendar(identifier: .gregorian); format.dateFormat = "yyyy-MM-dd"
        let start = format.string(from: from), end = format.string(from: to)
        let panel = NSSavePanel(); panel.nameFieldStringValue = "vpnmgr-events-\(start)_\(end).jsonl"
        guard panel.runModal() == .OK, let file = panel.url else { return }
        do {
            let data = try await api.data("/api/events/export?from=\(start)&to=\(end)")
            guard model.isCurrent(api), !Task.isCancelled else { return }
            try data.write(to: file, options: .atomic); try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: file.path)
            model.message = "运行记录已导出"; error = nil
        } catch { if model.isCurrent(api), !Task.isCancelled { self.error = error.localizedDescription } }
    }
}
