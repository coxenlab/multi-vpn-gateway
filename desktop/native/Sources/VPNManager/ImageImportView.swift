import SwiftUI
import UniformTypeIdentifiers

struct ImageImportTicket: Decodable {
    let id: String; let status: String; let progress: String?; let error: String?; let preview: ImageArchivePreview
}
struct ImageArchivePreview: Decodable {
    let images: [ArchiveImage]; let bytes: Int64; let sha256: String
    struct ArchiveImage: Decodable, Identifiable {
        let tags: [String]; let id: String; let architecture: String; let adapters: [String]
    }
}
struct ImageImportView: View {
    @EnvironmentObject var model: AppModel
    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                Label("导入镜像模板", systemImage: "square.and.arrow.down").font(.headline)
                Spacer()
                Button("选择文件…") { selectFile() }.disabled(model.busy.contains("__image-import") || model.importTicket != nil)
            }
            if model.busy.contains("__image-import"), model.importTicket == nil { ProgressView("正在读取并校验镜像包…") }
            else if let ticket = model.importTicket {
                ForEach(ticket.preview.images) { image in
                    VStack(alignment: .leading, spacing: 4) {
                        Text(image.adapters.joined(separator: "、"))
                        Text("Linux · \(image.architecture)").font(.caption).foregroundStyle(.secondary)
                        DisclosureGroup("镜像标签与摘要") {
                            Text(image.tags.joined(separator: "\n")).textSelection(.enabled)
                            Text(image.id).textSelection(.enabled)
                        }.font(.system(.caption, design: .monospaced))
                    }
                }
                Text(ByteCountFormatter.string(fromByteCount: ticket.preview.bytes, countStyle: .file)).foregroundStyle(.secondary)
                DisclosureGroup("文件校验值") { Text(ticket.preview.sha256).font(.system(.caption, design: .monospaced)).textSelection(.enabled) }
                if let error = ticket.error { Text(error).foregroundStyle(.red) }
                else { Text(ticket.progress ?? "等待确认").foregroundStyle(.secondary) }
                if let error = model.importError { Text(error).foregroundStyle(.red) }
                if ticket.status == "preview", model.system?.runtime?.ready != true { RuntimeStatusView() }
                HStack {
                    if ticket.status == "preview" {
                        Button("确认导入模板") { Task { await model.confirmImageImport() } }
                            .buttonStyle(.borderedProminent).disabled(model.system?.runtime?.ready != true || model.busy.contains("__image-import"))
                    }
                    if ticket.status == "loading" { ProgressView().controlSize(.small) }
                    Button("刷新状态") { Task { await model.refreshImageImport() } }.disabled(model.busy.contains("__image-refresh"))
                    if ticket.status != "loading" { Button(ticket.status == "preview" ? "取消预览" : "完成") { Task { await model.discardImageImport() } }.disabled(model.busy.contains("__image-import")) }
                }
                if ticket.status == "preview" {
                    Text("同名模板标签会更新，已有通道实例与数据保留。导入后自行创建通道并登录。").font(.caption).foregroundStyle(.secondary)
                    Text("请使用可信来源的干净模板；内容校验不代表来源认证，也不会检查镜像中是否带有个人账号数据。").font(.caption).foregroundStyle(.secondary)
                }
            } else {
                Text("将 Docker save 的 .tar 或 .tar.gz 拖到这里，或选择文件。先校验类型、架构与内容，再确认导入。").font(.callout).foregroundStyle(.secondary)
                if let error = model.importError { Text(error).foregroundStyle(.red) }
            }
        }.padding().background(.quaternary.opacity(0.3), in: RoundedRectangle(cornerRadius: 8))
            .dropDestination(for: URL.self) { urls, _ in
                guard urls.count == 1, let url = urls.first, url.isFileURL, model.importTicket == nil, !model.busy.contains("__image-import") else { return false }
                Task { await model.previewImage(url) }; return true
            }
    }
    private func selectFile() {
        let panel = NSOpenPanel(); panel.allowsMultipleSelection = false; panel.canChooseDirectories = false
        panel.allowedContentTypes = [UTType(filenameExtension: "tar"), UTType(filenameExtension: "gz")].compactMap { $0 }
        guard panel.runModal() == .OK, let file = panel.url else { return }
        Task { await model.previewImage(file) }
    }
}
