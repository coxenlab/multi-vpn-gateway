import SwiftUI

struct ContainerFeed: Decodable { let docker_available: Bool; let containers: [ContainerEntry] }
struct ContainerEntry: Decodable, Identifiable {
    let name: String; let role: String; let title: String?; let channel_id: String?; let channel_name: String?; let state: String; let restart_count: Int?; let crash_loop: Bool?; let image: String?
    var id: String { name }
    var label: String { channel_name ?? (name == "mihomo" ? "分流入口" : title ?? name) }
    var stateLabel: String { ["running":"运行中", "exited":"已停止", "created":"待启动", "missing":"未确认", "restarting":"重启中", "paused":"已暂停", "dead":"异常"][state] ?? "待确认" }
}
struct ContainersView: View {
    @EnvironmentObject var model: AppModel
    @Environment(\.dismiss) var dismiss
    @State private var report: ContainerFeed?
    @StateObject private var request = PageRequest()
    @State private var removing: ContainerEntry?
    @State private var repairing = false
    @State private var logs: Inspection?
    var body: some View {
        VStack(alignment: .leading) {
            HStack { Text("容器状态").font(.title2.bold()); Spacer(); if request.loading { ProgressView().controlSize(.small) }; Button("刷新") { Task { await load() } }.disabled(request.loading) }.padding()
            if let error = request.error { Text(error).foregroundStyle(.red).padding(.horizontal) }
            if report?.docker_available == false { Text("运行环境未连接；这里列出预期组件，不能据此判断实例是否丢失。").foregroundStyle(.secondary).padding(.horizontal) }
            List(report?.containers ?? []) { item in
                VStack(alignment: .leading, spacing: 8) {
                    HStack {
                        VStack(alignment: .leading, spacing: 4) { Text(item.label).font(.headline); Text(report?.docker_available == true ? item.stateLabel : "等待连接环境后核对").foregroundStyle(.secondary) }
                        Spacer()
                        if let id = item.channel_id { Button("查看通道") { model.createdChannelID = id; dismiss() } }
                        if item.role == "orphan" { Button("清理残留…", role: .destructive) { removing = item }.disabled(report?.docker_available != true || model.busy.contains("container-" + item.id)) }
                        if item.name == "mihomo" { Button("修复入口…") { repairing = true }.disabled(report?.docker_available != true || model.busy.contains("__heal")) }
                        Button("日志") { logs = Inspection(path: "/api/containers/\(item.name)/logs?tail=300", title: item.label + "日志") }.disabled(report?.docker_available != true)
                    }
                    if item.role == "replacement" { Text("此资源参与通道恢复，请在通道详情处理。").font(.caption).foregroundStyle(.secondary) }
                    if item.crash_loop == true { Label("检测到反复重启，请查看日志", systemImage: "exclamationmark.triangle").font(.caption).foregroundStyle(.orange) }
                    DisclosureGroup("详细信息") { Text(item.name).textSelection(.enabled); if let image = item.image { Text(image).textSelection(.enabled) }; if let count = item.restart_count { Text("重启次数：\(count)") } }.font(.caption)
                }.padding(.vertical, 8)
            }
        }.task { request.activate(); await load() }.onDisappear { request.suspend() }
            .pageRefresh(enabled: !request.loading) { Task { await load() } }
            .sheet(item: $logs) { item in VStack { TextEndpointView(path: item.path, title: item.title); Button("关闭") { logs = nil }.keyboardShortcut(.cancelAction).padding() }.frame(width: 650, height: 450) }
            .confirmationDialog("清理这个残留容器？", isPresented: Binding(get: { removing != nil }, set: { if !$0 { removing = nil } }), titleVisibility: .visible) {
                if let item = removing { Button("清理 \(item.name)", role: .destructive) { Task { await model.perform("/api/containers/\(item.name)", key: "container-" + item.id, method: "DELETE", success: "残留已清理"); await load() }; removing = nil } }
            } message: { Text("残留实例及其可写层将被删除。后台会再次核对归属，正在恢复的通道资源不能清理。") }
            .confirmationDialog("修复分流入口？", isPresented: $repairing, titleVisibility: .visible) {
                Button("修复入口") { Task { await model.perform("/api/system/heal-proxy", key: "__heal", success: "入口修复已执行，请核对连接"); await load() } }
            } message: { Text("将按当前配置修复入口服务，可能短暂影响经本应用转发的连接。") }
    }
    private func load() async {
        await request.run(model: model, operation: { try await $0.get("/api/containers", as: ContainerFeed.self) }) { report = $0 }
    }
}
