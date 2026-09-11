import Foundation
import Combine

struct BackupDocument: Sendable {
    static let maximumBytes = 16 * 1024 * 1024
    let data: Data
    let channelCount: Int
    let ruleCount: Int
    let names: [String]

    static func load(_ file: URL) async throws -> Self {
        let work = Task.detached(priority: .utility) {
            try Task.checkCancellation()
            let value = try file.resourceValues(forKeys: [.fileSizeKey, .isRegularFileKey])
            guard value.isRegularFile == true, let size = value.fileSize, size > 0, size <= maximumBytes else {
                throw APIError(message: "请选择非空且不超过 16 MiB 的配置备份。")
            }
            let input = try FileHandle(forReadingFrom: file)
            defer { try? input.close() }
            var bytes = Data()
            while let chunk = try input.read(upToCount: 64 * 1024), !chunk.isEmpty {
                try Task.checkCancellation()
                guard bytes.count + chunk.count <= maximumBytes else { throw APIError(message: "配置备份不能超过 16 MiB。") }
                bytes.append(chunk)
            }
            guard let document = (try? JSONSerialization.jsonObject(with: bytes)) as? [String: Any],
                  document["kind"] as? String == "vpnmgr-export", let channels = document["channels"] as? [Any] else {
                throw APIError(message: "不是有效的配置备份，请选择本应用导出的 JSON 文件。")
            }
            try Task.checkCancellation()
            let objects = channels.compactMap { $0 as? [String: Any] }
            return Self(data: bytes, channelCount: channels.count,
                ruleCount: objects.reduce(0) { $0 + (($1["rules"] as? [Any])?.count ?? 0) },
                names: objects.prefix(8).map { $0["name"] as? String ?? "未命名通道" })
        }
        return try await withTaskCancellationHandler(operation: { try await work.value }, onCancel: { work.cancel() })
    }
}

struct BackupImportReport: Decodable {
    struct Skipped: Decodable { let name: String; let reason: String }
    let ok: Bool
    let imported: [String]
    let skipped: [Skipped]
    let deferred: Bool?
    let rules_applied: Bool?
    let config_application: ConfigApplication?
    var pending: Bool { deferred != true && (rules_applied == false || config_application?.pending == true) }
}

@MainActor final class BackupImportState: ObservableObject {
    @Published private(set) var document: BackupDocument?
    @Published private(set) var report: BackupImportReport?
    @Published private(set) var loading = false
    @Published private(set) var submitting = false
    @Published private(set) var error: String?
    private var generation = UUID()
    private var client: LocalAPI?
    private var readTask: Task<BackupDocument, Error>?

    func cancel() {
        generation = UUID(); readTask?.cancel(); readTask = nil
        loading = false; document = nil; client = nil
    }
    func connectionChanged() {
        cancel(); report = nil
        error = "本地服务连接已变化，请关闭后重新选择备份；若已经提交，请先刷新通道列表核对结果。"
    }
    func prepare(file: URL, model: AppModel, loader: @escaping @Sendable (URL) async throws -> BackupDocument = { try await BackupDocument.load($0) }) async {
        guard !submitting else { return }
        guard let client = model.api, model.isCurrent(client) else {
            cancel(); report = nil; error = "本地服务尚未就绪，请关闭后重试。"; return
        }
        cancel(); report = nil; error = nil; loading = true
        let current = generation
        let scoped = file.startAccessingSecurityScopedResource()
        defer {
            if scoped { file.stopAccessingSecurityScopedResource() }
            if generation == current { loading = false; readTask = nil }
        }
        let task = Task { try await loader(file) }; readTask = task
        do {
            let document = try await task.value
            guard generation == current, model.isCurrent(client), !Task.isCancelled else { return }
            self.document = document; self.client = client
        } catch {
            if generation == current, model.isCurrent(client), !(error is CancellationError) { self.error = error.localizedDescription }
        }
    }
    func submit(model: AppModel) async {
        guard !submitting, let document, let client, model.isCurrent(client) else { return }
        let current = generation
        submitting = true; error = nil
        // The request is owned until it returns. Leaving the view doesn't retry or undo it.
        defer { submitting = false }
        do {
            let report = try await client.importConfig(document.data)
            if generation == current, model.isCurrent(client) { self.report = report; self.document = nil }
        } catch {
            if generation == current, model.isCurrent(client) {
                self.document = nil
                self.error = "\(error.localizedDescription)\n请先刷新通道列表核对结果，不要直接重复导入。"
            }
        }
        if model.isCurrent(client) { await model.refresh() }
    }
}
