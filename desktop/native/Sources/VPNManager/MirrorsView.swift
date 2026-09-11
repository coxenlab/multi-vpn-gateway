import SwiftUI

struct Mirror: Decodable, Identifiable { let id: Int; let host: String; let priority: Int; let enabled: Int }
struct MirrorsView: View {
    @EnvironmentObject var model: AppModel
    @State private var mirrors: [Mirror] = []
    @State private var host = ""
    @State private var results: [Int: String] = [:]
    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("镜像下载源").font(.title2.bold())
            Text("按优先级尝试下载，不影响正在使用的通道。").foregroundStyle(.secondary)
            HStack { TextField("镜像源域名", text: $host).textFieldStyle(.roundedBorder); Button("添加") { Task { if await model.perform("/api/mirrors", key: "__mirrors", body: ["host": host], success: "下载源已添加") { host = ""; await load() } } }.disabled(host.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty) }
            List(mirrors) { mirror in
                HStack {
                    Toggle(mirror.host, isOn: Binding(get: { mirror.enabled == 1 }, set: { value in Task { await model.perform("/api/mirrors/\(mirror.id)", key: "mirror-\(mirror.id)", method: "PATCH", body: ["enabled": value], success: "下载源已保存"); await load() } })).disabled(model.busy.contains("mirror-\(mirror.id)"))
                    Spacer(); Text(results[mirror.id] ?? "优先级 \(mirror.priority)").font(.caption).foregroundStyle(.secondary)
                    Button("检测") { Task { await test(mirror) } }.disabled(model.busy.contains("mirror-test-\(mirror.id)"))
                    Button(role: .destructive) { Task { await model.perform("/api/mirrors/\(mirror.id)", key: "mirror-\(mirror.id)", method: "DELETE", success: "下载源已移除"); await load() } } label: { Image(systemName: "trash") }.buttonStyle(.borderless)
                }.contextMenu {
                    Button("优先使用此源") { Task { await model.perform("/api/mirrors/\(mirror.id)", key: "mirror-\(mirror.id)", method: "PATCH", body: ["priority": (mirrors.map(\.priority).min() ?? 0) - 1], success: "下载顺序已保存"); await load() } }
                }
            }
        }.padding(20).task { await load() }
    }
    private func load() async {
        guard let api = model.api else { return }
        do { mirrors = try await api.get("/api/mirrors", as: [Mirror].self) } catch { model.error = error.localizedDescription }
    }
    private func test(_ mirror: Mirror) async {
        guard let api = model.api else { return }
        let key = "mirror-test-\(mirror.id)"; model.busy.insert(key); defer { model.busy.remove(key) }
        do { let result = try await api.write("/api/mirrors/test", body: ["host": mirror.host]); results[mirror.id] = result["reachable"] as? Bool == true ? "可访问 · \(result["latency_ms"] as? Int ?? 0) ms" : "暂不可访问" }
        catch { results[mirror.id] = "检测失败" }
    }
}
