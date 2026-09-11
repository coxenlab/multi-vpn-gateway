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
    @StateObject private var request = PageRequest()
    var body: some View {
        VStack(alignment: .leading) {
            HStack { Text("环境检查").font(.title2.bold()); Spacer(); Button("重新检查") { Task { await load() } }.disabled(request.loading) }.padding()
            if request.loading { ProgressView("正在检查…").frame(maxWidth: .infinity) }
            if let error = request.error { Text(error).foregroundStyle(.red).padding() }
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
        }.task { request.activate(); await load() }.onDisappear { request.suspend() }
            .pageRefresh(enabled: !request.loading) { Task { await load() } }
    }
    private func load() async {
        await request.run(model: model, operation: { try await $0.get("/api/preflight?scope=full", as: CheckReport.self) }) { checks = $0.checks }
    }
}
struct ImagesView: View {
    @EnvironmentObject var model: AppModel
    @State private var images: [ImageEntry] = []
    @StateObject private var request = PageRequest()
    var body: some View {
        VStack(alignment: .leading) {
            HStack { Text("镜像与资源").font(.title2.bold()); Spacer(); if request.loading { ProgressView().controlSize(.small) }; Button("刷新") { Task { await load() } }.disabled(request.loading) }.padding()
            ScrollView { ImageImportView() }.frame(maxHeight: model.importTicket == nil ? 150 : 280).padding(.horizontal)
            if let error = request.error { Text(error).foregroundStyle(.red).padding() }
            List(images) { item in
                HStack {
                    VStack(alignment: .leading, spacing: 5) {
                        Text(item.title).font(.headline)
                        Text(model.imageProgress[item.id] ?? (item.present == true ? "已准备" : item.present == nil ? "连接运行环境后检查" : item.kind == "build" ? "需要导入本产品镜像" : "尚未下载")).font(.callout).foregroundStyle(.secondary)
                        DisclosureGroup("详细信息") { Text(item.image).font(.system(.caption, design: .monospaced)).textSelection(.enabled) }
                    }
                    Spacer()
                    if model.busy.contains("image-" + item.id) { ProgressView().controlSize(.small) }
                    else if model.imageTasks[item.id] != nil { Button("查看进度") { Task { await model.downloadImage(item); await load() } } }
                    else if item.kind == "pull", item.present != true { Button("下载") { Task { await model.downloadImage(item); await load() } }.disabled(model.system?.runtime?.ready != true) }
                }.padding(.vertical, 8)
            }
        }.task { request.activate(); await load() }.onChange(of: model.importTicket?.status) { _, status in if status == "done" { Task { await load() } } }.onDisappear { request.suspend() }
            .pageRefresh(enabled: !request.loading) { Task { await load() } }
    }
    private func load() async {
        await request.run(model: model, operation: { try await $0.get("/api/images", as: ImageReport.self) }) { images = $0.images }
    }
}
