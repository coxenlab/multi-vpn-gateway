import Foundation
import Combine

struct VPNVersion: Decodable, Identifiable {
    var id: String { tag }
    let tag: String
    let usable_here: Bool
}
struct VersionFeed: Decodable { let versions: [VPNVersion] }

@MainActor final class VersionChoices: ObservableObject {
    @Published private(set) var items: [VPNVersion] = []
    @Published private(set) var loading = false
    @Published private(set) var error: String?
    @Published private(set) var loaded = false
    private var generation = UUID()
    func load(versioned: Bool, fetch: () async throws -> VersionFeed) async {
        guard !Task.isCancelled else { return }
        generation = UUID(); let current = generation
        items = []; error = nil; loaded = false; loading = versioned
        guard versioned else { return }
        defer { if generation == current { loading = false } }
        do {
            let feed = try await fetch()
            guard generation == current, !Task.isCancelled else { return }
            var seen = Set<String>()
            items = feed.versions.filter { !$0.tag.isEmpty && seen.insert($0.tag).inserted }
            loaded = true
        } catch {
            guard generation == current, !Task.isCancelled else { return }
            self.error = "版本列表读取失败，可重试或填写已知版本。"
        }
    }
    func selection(current: String, existing: String?) -> String {
        if current == existing || items.contains(where: { $0.tag == current && $0.usable_here }) { return current }
        return items.first(where: { $0.usable_here })?.tag ?? ""
    }
    func permits(_ version: String, existing: String?) -> Bool {
        !loading && !version.isEmpty && (!loaded || version == existing || items.contains { $0.tag == version && $0.usable_here })
    }
}

func readTextConfiguration(_ file: URL) throws -> String {
    let scoped = file.startAccessingSecurityScopedResource()
    defer { if scoped { file.stopAccessingSecurityScopedResource() } }
    guard try file.resourceValues(forKeys: [.isRegularFileKey]).isRegularFile == true else { throw APIError(message: "请选择文本配置文件。") }
    let input = try FileHandle(forReadingFrom: file); defer { try? input.close() }
    let data = try input.read(upToCount: 1024 * 1024 + 1) ?? Data()
    guard !data.isEmpty, data.count <= 1024 * 1024, let text = String(data: data, encoding: .utf8) else { throw APIError(message: "请选择不超过 1 MB 的非空文本配置文件。") }
    return text
}
