import SwiftUI

struct ConnectionFeed: Decodable {
    let downloadTotal: Int64?; let uploadTotal: Int64?; let connections: [TrafficConnection]?
}
struct TrafficConnection: Decodable, Identifiable {
    let id: String; let metadata: Metadata; let chains: [String]?; let download: Int64?; let upload: Int64?
    struct Metadata: Decodable { let host: String?; let destinationIP: String?; let destinationPort: String?; let network: String? }
}
struct MonitorView: View {
    @EnvironmentObject var model: AppModel
    @State private var feed: ConnectionFeed?
    @State private var error: String?
    @State private var paused = false
    @State private var search = ""
    @State private var previous: (at: TimeInterval, download: Int64, upload: Int64)?
    @State private var downloadRate: Int64?
    @State private var uploadRate: Int64?
    private var rows: [TrafficConnection] { (feed?.connections ?? []).filter { search.isEmpty || ($0.metadata.host ?? "").localizedCaseInsensitiveContains(search) || ($0.metadata.destinationIP ?? "").localizedCaseInsensitiveContains(search) || outbound($0.chains ?? []).localizedCaseInsensitiveContains(search) } }
    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack {
                Text("↓ \(downloadRate.map { size($0) + "/s" } ?? "—")    ↑ \(uploadRate.map { size($0) + "/s" } ?? "—")").monospacedDigit()
                Text("\(feed?.connections?.count ?? 0) 个连接").foregroundStyle(.secondary)
                Spacer(); Button("导出当前列表…") { export() }.disabled(rows.isEmpty)
                Button(paused ? "继续刷新" : "暂停刷新") { paused.toggle(); previous = nil; downloadRate = nil; uploadRate = nil }
            }
            TextField("搜索目标地址或通道", text: $search).textFieldStyle(.roundedBorder)
            if model.system?.runtime?.ready != true { ContentUnavailableView("运行环境休眠中", systemImage: "moon", description: Text("连接通道后，这里会显示当前流量。")) }
            else if let error { ContentUnavailableView("流量暂不可用", systemImage: "wifi.exclamationmark", description: Text(error)) }
            else {
                Table(rows) {
                    TableColumn("目标") { c in Text(c.metadata.host?.isEmpty == false ? c.metadata.host! : c.metadata.destinationIP ?? "未知").textSelection(.enabled) }
                    TableColumn("通道") { c in Text(outbound(c.chains ?? [])) }
                    TableColumn("下载") { Text(size($0.download)).monospacedDigit() }
                    TableColumn("上传") { Text(size($0.upload)).monospacedDigit() }
                }
            }
            Text(paused ? "已暂停界面刷新，网络连接保持。" : "只在此页面可见时刷新当前连接。").font(.caption).foregroundStyle(.secondary)
        }.padding(20).task {
            while !Task.isCancelled {
                if model.visible && !paused && model.system?.runtime?.ready == true, let api = model.api {
                    do {
                        let next = try await api.get("/api/connections", as: ConnectionFeed.self)
                        let now = ProcessInfo.processInfo.systemUptime
                        downloadRate = nil; uploadRate = nil
                        if let down = next.downloadTotal, let up = next.uploadTotal {
                            if let old = previous, now > old.at, now - old.at < 10, down >= old.download, up >= old.upload {
                                downloadRate = Int64(Double(down - old.download) / (now - old.at)); uploadRate = Int64(Double(up - old.upload) / (now - old.at))
                            }
                            previous = (now, down, up)
                        } else { previous = nil }
                        feed = next; error = nil
                    } catch { self.error = error.localizedDescription; previous = nil; downloadRate = nil; uploadRate = nil }
                }
                do { try await Task.sleep(for: .seconds(2)) } catch { return }
            }
        }
    }
    private func size(_ bytes: Int64?) -> String { ByteCountFormatter.string(fromByteCount: bytes ?? 0, countStyle: .binary) }
    private func export() {
        let snapshot = rows
        let panel = NSSavePanel(); panel.nameFieldStringValue = "vpnmgr-connections.csv"
        guard panel.runModal() == .OK, let file = panel.url else { return }
        let values = [["目标", "IP", "端口", "协议", "通道", "下载字节", "上传字节"]] + snapshot.map { c in
            [c.metadata.host ?? "", c.metadata.destinationIP ?? "", c.metadata.destinationPort ?? "", c.metadata.network ?? "", outbound(c.chains ?? []), String(c.download ?? 0), String(c.upload ?? 0)]
        }
        do {
            let text = "\u{feff}" + values.map { $0.map(csvField).joined(separator: ",") }.joined(separator: "\r\n")
            try Data(text.utf8).write(to: file, options: .atomic); try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: file.path)
            model.message = "已导出 \(snapshot.count) 条连接记录"
        } catch { self.error = error.localizedDescription }
    }
    private func outbound(_ chains: [String]) -> String {
        if let name = chains.first(where: { $0.hasPrefix("ch-") }), let ch = model.channels.first(where: { "ch-" + $0.id == name }) { return ch.name }
        return chains.last == "DIRECT" ? "直连" : chains.last ?? "—"
    }
}

func csvField(_ input: String) -> String {
    let stripped = input.trimmingCharacters(in: .whitespacesAndNewlines)
    let value = stripped.first.map { "=+-@".contains($0) } == true ? "'" + input : input
    return "\"" + value.replacingOccurrences(of: "\"", with: "\"\"") + "\""
}

struct TextEndpointView: View {
    @EnvironmentObject var model: AppModel
    let path: String; let title: String
    @State private var text = ""
    @State private var loading = false
    var body: some View {
        VStack(alignment: .leading) {
            HStack { Text(title).font(.headline); Spacer(); if loading { ProgressView().controlSize(.small) }; Button("刷新") { Task { await load() } }.disabled(loading) }
            ScrollView { Text(text.isEmpty ? "暂无记录" : text).font(.system(.caption, design: .monospaced)).textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading) }
        }.padding().task(id: path) { await load() }
    }
    private func load() async {
        guard let api = model.api else { return }; loading = true; defer { loading = false }
        do {
            let bytes = try await api.data(path)
            if let lines = try? JSONDecoder().decode([String].self, from: bytes) { text = lines.joined(separator: "\n") }
            else if let object = (try? JSONSerialization.jsonObject(with: bytes)) as? [String: Any], let lines = object["lines"] as? [String] { text = lines.joined(separator: "\n") }
            else if let value = try? JSONSerialization.jsonObject(with: bytes), let pretty = try? JSONSerialization.data(withJSONObject: value, options: [.prettyPrinted, .sortedKeys]) { text = String(decoding: pretty, as: UTF8.self) }
            else { text = String(decoding: bytes, as: UTF8.self) }
        } catch { text = "读取失败：\(error.localizedDescription)" }
    }
}
