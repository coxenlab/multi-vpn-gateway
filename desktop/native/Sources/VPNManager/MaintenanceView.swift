import SwiftUI

struct CheckReport: Decodable { let checks: [EnvironmentCheck]; let overall: String }
struct EnvironmentCheck: Decodable, Identifiable {
    let id: String; let title: String; let status: String; let detail: String; let fix: Fix?
    struct Fix: Decodable { let kind: String; let action: String; let label: String; let params: [String: String]? }
}
struct ImageReport: Decodable { let images: [ImageEntry] }
struct ImageEntry: Decodable, Identifiable {
    var id: String { image }
    let image: String; let title: String; let kind: String; let present: Bool?
}
struct PullStatus: Decodable { let status: String; let progress: String?; let error: String? }

struct EnvironmentView: View {
    @EnvironmentObject var model: AppModel
    @State private var checks: [EnvironmentCheck] = []
    @State private var loading = false
    @State private var error: String?
    var body: some View {
        VStack(alignment: .leading) {
            HStack { Text("环境检查").font(.title2.bold()); Spacer(); Button("重新检查") { Task { await load() } }.disabled(loading) }.padding()
            if loading { ProgressView("正在检查…").frame(maxWidth: .infinity) }
            if let error { Text(error).foregroundStyle(.red).padding() }
            List(checks) { check in
                HStack(alignment: .top, spacing: 12) {
                    Image(systemName: check.status == "pass" ? "checkmark.circle.fill" : check.status == "skip" ? "minus.circle" : "exclamationmark.triangle.fill")
                        .foregroundStyle(check.status == "pass" ? .green : check.status == "skip" ? .secondary : .orange)
                    VStack(alignment: .leading, spacing: 5) { Text(check.title).font(.headline); Text(check.detail).foregroundStyle(.secondary).textSelection(.enabled) }
                    Spacer()
                    if let fix = check.fix, fix.kind == "auto", fix.action == "create_network" {
                        Button(fix.label) { Task { await model.perform("/api/preflight/fix/\(fix.action)", key: check.id, body: fix.params ?? [:]); await load() } }.disabled(model.system?.runtime?.ready != true || model.busy.contains(check.id))
                    }
                }.padding(.vertical, 8)
            }
            if model.system?.runtime?.ready != true { Text("环境休眠时仅检查现有设置。需要修复时，请先明确连接运行环境。").font(.caption).foregroundStyle(.secondary).padding() }
        }.task { await load() }
    }
    private func load() async {
        guard let api = model.api, !loading else { return }; loading = true; defer { loading = false }
        do { checks = try await api.get("/api/preflight?scope=full", as: CheckReport.self).checks; error = nil }
        catch { self.error = error.localizedDescription }
    }
}
struct ImagesView: View {
    @EnvironmentObject var model: AppModel
    @State private var images: [ImageEntry] = []
    @State private var error: String?
    var body: some View {
        VStack(alignment: .leading) {
            HStack { Text("镜像与资源").font(.title2.bold()); Spacer(); Button("刷新") { Task { await load() } } }.padding()
            if let error { Text(error).foregroundStyle(.red).padding() }
            List(images) { item in
                HStack {
                    VStack(alignment: .leading, spacing: 5) {
                        Text(item.title).font(.headline)
                        Text(model.imageProgress[item.id] ?? (item.present == true ? "已准备" : item.present == nil ? "连接运行环境后检查" : item.kind == "build" ? "需要导入本产品镜像" : "尚未下载")).font(.callout).foregroundStyle(.secondary)
                        DisclosureGroup("详细信息") { Text(item.image).font(.system(.caption, design: .monospaced)).textSelection(.enabled) }
                    }
                    Spacer()
                    if model.busy.contains("image-" + item.id) { ProgressView().controlSize(.small) }
                    else if item.kind == "pull", item.present != true { Button("下载") { Task { await model.downloadImage(item); await load() } }.disabled(model.system?.runtime?.ready != true) }
                }.padding(.vertical, 8)
            }
        }.task { await load() }
    }
    private func load() async {
        guard let api = model.api else { return }
        do { images = try await api.get("/api/images", as: ImageReport.self).images; error = nil }
        catch { self.error = error.localizedDescription }
    }
}
