import Foundation

struct Channel: Decodable, Identifiable, Hashable {
    let id: String
    var name: String
    var vpn_type: String
    var server: String
    var username: String
    var status: String
    var configured_status: String?
    var login_method: String
    var ec_ver: String?
    var probe_url: String
    var container_id: String?
    var routing_enabled: Bool
    var stop_pending: Bool?
    var latency_ms: Int?
    var domains: [Rule]
    var ips: [Rule]
    var replacement: Replacement?
    var rules: [Rule] { domains + ips }
    var needsStart: Bool { stop_pending == true || replacement?.phase == "queued" || ["stopped", "down", "error"].contains(status) }
    func statusLabel(runtime: Runtime?) -> String {
        if stop_pending == true { return "停用待确认" }
        if runtime?.ready == false && configured_status != "stopped" { return "待连接" }
        return ["logged_in": "已连接", "running": "待登录", "creating": "准备中", "stopped": "已停止", "down": "连接中断", "error": "需要处理"][status] ?? "待确认"
    }
}
struct Replacement: Decodable, Hashable { let phase: String; let can_restore: Bool }
struct Rule: Decodable, Identifiable, Hashable {
    let id: Int; let kind: String; let pattern: String; let enabled: Int; let note: String?; let locked: Int?
}
struct Runtime: Decodable {
    let phase: String; let detail: String?; let error: String?
    var ready: Bool { ["ready", "waiting"].contains(phase) }
    var label: String { ["dormant":"按需运行", "starting":"准备连接", "ready":"运行中", "waiting":"等待空闲释放", "releasing":"释放中", "failed":"需要重试"][phase] ?? "待确认" }
}
struct SystemStatus: Decodable {
    let runtime: Runtime?; let routing_off: Bool?; let self_heal_enabled: Bool?
    let mihomo_status: String?; let config_application: ConfigApplication?
}
struct Adapter: Decodable, Identifiable {
    var id: String { key }
    let key: String; let label: String; let desc: String; let runtime: String; let versioned: Bool
    let login_modes: [String]; let inputs: [InputField]; let notice: String?
}
struct InputField: Decodable, Identifiable {
    var id: String { key }
    let key: String; let label: String; let type: String; let secret: Bool?; let required: Bool?
}
struct APIError: LocalizedError {
    let message: String
    var statusCode: Int? = nil
    var saved = false
    var errorDescription: String? { message }
}

actor LocalAPI {
    typealias Transport = @Sendable (URLRequest) async throws -> (Data, URLResponse)
    private let base: URL
    private let session: URLSession
    private let transport: Transport?
    init(port: Int, transport: Transport? = nil) {
        base = URL(string: "http://127.0.0.1:\(port)")!
        self.transport = transport
        let configuration = URLSessionConfiguration.ephemeral
        configuration.timeoutIntervalForRequest = 25
        configuration.timeoutIntervalForResource = 1200
        configuration.connectionProxyDictionary = [:]
        session = URLSession(configuration: configuration)
    }
    func invalidate() { session.invalidateAndCancel() }
    func url(_ path: String) -> URL { URL(string: path, relativeTo: base)!.absoluteURL }
    func data(_ path: String, method: String = "GET", body: [String: Any]? = nil, long: Bool = false) async throws -> Data {
        var request = URLRequest(url: url(path))
        request.httpMethod = method; request.timeoutInterval = long ? 1200 : 25
        if let body {
            request.httpBody = try JSONSerialization.data(withJSONObject: body)
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        }
        let result: (Data, URLResponse)
        if let transport { result = try await transport(request) }
        else { result = try await session.data(for: request) }
        let (data, response) = result
        guard let http = response as? HTTPURLResponse else { throw APIError(message: "本地服务响应无法识别") }
        guard (200..<300).contains(http.statusCode) else {
            let error = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
            throw APIError(message: error?["error"] as? String ?? error?["detail"] as? String ?? "操作未完成（\(http.statusCode)）", statusCode: http.statusCode, saved: error?["saved"] as? Bool == true)
        }
        return data
    }
    func upload(_ path: String, file: URL) async throws {
        let staged = try await InstallerUpload.prepare(file)
        defer { staged.remove() }
        var request = URLRequest(url: url(path)); request.httpMethod = "POST"; request.timeoutInterval = 1200
        request.setValue("multipart/form-data; boundary=\(staged.boundary)", forHTTPHeaderField: "Content-Type")
        let (data, response) = try await session.upload(for: request, fromFile: staged.file)
        let value = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
        guard let http = response as? HTTPURLResponse, (200..<300).contains(http.statusCode), value?["ok"] as? Bool == true else {
            throw APIError(message: value?["error"] as? String ?? value?["detail"] as? String ?? "上传结果未确认，请刷新核对。")
        }
    }
    func get<T: Decodable>(_ path: String, as type: T.Type) async throws -> T { try JSONDecoder().decode(type, from: await data(path)) }
    func previewImage(file: URL) async throws -> ImageImportTicket {
        let size = try file.resourceValues(forKeys: [.fileSizeKey, .isRegularFileKey])
        guard size.isRegularFile == true, let bytes = size.fileSize, bytes > 0, Int64(bytes) <= 12 * 1024 * 1024 * 1024 else { throw APIError(message: "请选择不超过 12 GiB 的镜像归档文件") }
        var request = URLRequest(url: url("/api/images/imports")); request.httpMethod = "POST"; request.timeoutInterval = 1200
        request.setValue("application/octet-stream", forHTTPHeaderField: "Content-Type")
        let (data, response) = try await session.upload(for: request, fromFile: file)
        guard let http = response as? HTTPURLResponse, (200..<300).contains(http.statusCode) else {
            let value = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
            throw APIError(message: value?["error"] as? String ?? value?["detail"] as? String ?? "镜像校验未完成，请重新选择文件")
        }
        return try JSONDecoder().decode(ImageImportTicket.self, from: data)
    }
    func write(_ path: String, method: String = "POST", body: [String: Any]? = nil) async throws -> [String: Any] {
        let bytes = try await data(path, method: method, body: body, long: true)
        return (try? JSONSerialization.jsonObject(with: bytes)) as? [String: Any] ?? [:]
    }
}
