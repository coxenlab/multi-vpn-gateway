import SwiftUI

struct Mirror: Decodable, Identifiable { let id: Int; let host: String; let priority: Int; let enabled: Int }
struct MirrorsView: View {
    @EnvironmentObject var model: AppModel
    @State private var mirrors: [Mirror] = []
    @State private var host = ""
    @State private var results: [Int: String] = [:]
    @StateObject private var request = PageRequest()
    @State private var generation = UUID()
    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("镜像下载源").font(.title2.bold())
            Text("按优先级尝试下载，不影响正在使用的通道。").foregroundStyle(.secondary)
            HStack {
                TextField("镜像源域名", text: $host).textFieldStyle(.roundedBorder).onSubmit { Task { await add() } }
                Button("添加") { Task { await add() } }.disabled(host.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || model.busy.contains("__mirrors"))
                if request.loading || model.busy.contains("__mirrors") { ProgressView().controlSize(.small) }
                Button("刷新") { Task { await load() } }.disabled(request.loading)
            }
            if let error = request.error { Text(error).foregroundStyle(.red) }
            List(mirrors) { mirror in
                HStack {
                    Toggle(mirror.host, isOn: Binding(get: { mirror.enabled == 1 }, set: { value in Task { await model.perform("/api/mirrors/\(mirror.id)", key: "mirror-\(mirror.id)", method: "PATCH", body: ["enabled": value], success: "下载源已保存"); await load() } })).disabled(model.busy.contains("mirror-\(mirror.id)"))
                    Spacer(); Text(results[mirror.id] ?? "优先级 \(mirror.priority)").font(.caption).foregroundStyle(.secondary)
                    Button("检测") { Task { await test(mirror) } }.disabled(model.busy.contains("mirror-test-\(mirror.id)"))
                    Button(role: .destructive) { Task { await model.perform("/api/mirrors/\(mirror.id)", key: "mirror-\(mirror.id)", method: "DELETE", success: "下载源已移除"); await load() } } label: { Image(systemName: "trash") }.buttonStyle(.borderless).disabled(model.busy.contains("mirror-\(mirror.id)")).accessibilityLabel("移除 \(mirror.host)")
                }.contextMenu {
                    Button("优先使用此源") { Task { await model.perform("/api/mirrors/\(mirror.id)", key: "mirror-\(mirror.id)", method: "PATCH", body: ["priority": (mirrors.map(\.priority).min() ?? 0) - 1], success: "下载顺序已保存"); await load() } }.disabled(model.busy.contains("mirror-\(mirror.id)"))
                }
            }
        }.padding(20).task { request.activate(); await load() }.onDisappear { request.suspend(); generation = UUID() }
            .pageRefresh(enabled: !request.loading) { Task { await load() } }
    }
    private func add() async {
        let submitted = host
        guard !submitted.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return }
        if await model.perform("/api/mirrors", key: "__mirrors", body: ["host": submitted], success: "下载源已添加") {
            if host == submitted { host = "" }
            await load()
        }
    }
    private func load() async {
        await request.run(model: model, operation: { try await $0.get("/api/mirrors", as: [Mirror].self) }) { mirrors = $0 }
    }
    private func test(_ mirror: Mirror) async {
        let key = "mirror-test-\(mirror.id)", current = generation
        guard request.isActive, let api = model.api, model.isCurrent(api), !model.busy.contains(key) else { return }
        model.busy.insert(key); defer { if model.isCurrent(api) { model.busy.remove(key) } }
        do {
            let result = try await api.write("/api/mirrors/test", body: ["host": mirror.host])
            guard model.isCurrent(api), generation == current, !Task.isCancelled else { return }
            results[mirror.id] = result["reachable"] as? Bool == true ? "可访问 · \(result["latency_ms"] as? Int ?? 0) ms" : "暂不可访问"
        } catch { if model.isCurrent(api), generation == current, !Task.isCancelled { results[mirror.id] = "检测失败" } }
    }
}
