import Foundation

struct Channel: Decodable, Identifiable, Hashable {
    let id: String
    var name: String
    var vpn_type: String
    var server: String
    var username: String
    var status: String
    var configured_status: String?
    var container_state: String?
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
    let progress_age_seconds: UInt64?
    var ready: Bool { ["ready", "waiting"].contains(phase) }
    var working: Bool { ["starting", "releasing", "closing"].contains(phase) }
    var canConnect: Bool { ["dormant", "failed"].contains(phase) }
    var label: String { ["dormant":"按需运行", "starting":"准备连接", "ready":"运行中", "waiting":"等待空闲释放", "releasing":"释放中", "failed":"连接未完成", "closing":"正在断开"][phase] ?? "待确认" }
    var notice: String? {
        if phase == "failed" { return error.flatMap { $0.isEmpty ? nil : $0 } ?? "运行环境尚未就绪。请查看运行诊断，核对后重试连接。" }
        return detail.flatMap { $0.isEmpty ? nil : $0 }
    }
    var quietPreparation: Bool { phase == "starting" && (progress_age_seconds ?? 0) >= 90 }
}
struct SystemStatus: Decodable {
    let runtime: Runtime?; let routing_off: Bool?; let self_heal_enabled: Bool?
    let mihomo_status: String?; let config_application: ConfigApplication?
    let host_integrations_available: Bool?
    let gateway_health: String?; let gateway_checked_at_ms: Int64?
    let proxy_port_reachable: Bool?; let healing: Bool?; let gave_up: Bool?
    let vm_egress_dead: Bool?; let egress_guard_applied: Bool?
}
struct ConfigApplication: Decodable {
    let available: Bool
    let pending: Bool
    let verified_at: Int64?
    let last_error: String?
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

/// An accepted HTTP response can still describe a failed or partly applied operation.
func operationMessage(_ value: [String: Any], fallback: String) throws -> String {
    if value["ok"] as? Bool == false { throw APIError(message: value["error"] as? String ?? "操作未完成，请刷新核对状态。", saved: value["saved"] as? Bool == true) }
    if value["stop_pending"] as? Bool == true { return "已保存停用，实际停止尚待确认" }
    if value["deferred"] as? Bool == true { return "已保存，连接后应用" }
    if value["rules_applied"] as? Bool == false || (value["config_application"] as? [String: Any])?["pending"] as? Bool == true { return "设置已保存，规则同步尚未确认" }
    return fallback
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
    func data(_ path: String, method: String = "GET", body: [String: Any]? = nil, rawBody: Data? = nil, long: Bool = false) async throws -> Data {
        var request = URLRequest(url: url(path))
        request.httpMethod = method; request.timeoutInterval = long ? 1200 : 25
        if let rawBody {
            request.httpBody = rawBody
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        } else if let body {
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
    func get<T: Decodable>(_ path: String, as type: T.Type) async throws -> T { try JSONDecoder().decode(type, from: await data(path)) }
    func write(_ path: String, method: String = "POST", body: [String: Any]? = nil) async throws -> [String: Any] {
        let bytes = try await data(path, method: method, body: body, long: true)
        return (try? JSONSerialization.jsonObject(with: bytes)) as? [String: Any] ?? [:]
    }
}
