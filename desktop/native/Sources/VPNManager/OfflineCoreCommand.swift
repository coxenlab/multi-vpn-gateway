import Foundation

struct OfflineLaunchContext: Sendable {
    let executable: URL
    let environment: [String: String]
}

struct UpgradeSwitchStatus: Decodable {
    let phase: String
    let profile: String
    let source: String
    let target: String
    let retained_directory: String
    let runtime_verified: Bool
    var label: String { ["prepared":"等待完成切换", "activating":"切换中断，等待恢复", "active":"配置已切换", "rolling_back":"回退中断，等待恢复", "rolled_back":"配置已回退"][phase] ?? "需要核对" }
    var canResume: Bool { ["prepared", "activating", "rolling_back"].contains(phase) }
    var canRollback: Bool { ["prepared", "activating", "active", "rolling_back"].contains(phase) }
}

enum UpgradeAction: String, Identifiable {
    case activate, resume, rollback, finish
    var id: String { rawValue }
    var command: String { ["activate":"--activate-upgrade", "resume":"--resume-upgrade", "rollback":"--rollback-upgrade", "finish":"--finish-upgrade"][rawValue]! }
    var title: String { ["activate":"启用升级副本", "resume":"恢复上次切换", "rollback":"回退到切换前配置", "finish":"完成升级记录"][rawValue]! }
}

func runOfflineCore<Result: Decodable>(context: OfflineLaunchContext, arguments: [String], as: Result.Type) throws -> Result {
    let process = Process(); process.executableURL = context.executable; process.environment = context.environment
    process.arguments = arguments; process.standardInput = FileHandle.nullDevice
    let output = Pipe(); process.standardOutput = output; process.standardError = output
    try process.run()
    var bytes = Data()
    while true {
        let chunk = output.fileHandleForReading.readData(ofLength: 4096)
        if chunk.isEmpty { break }
        if bytes.count < 65536 { bytes.append(chunk.prefix(65536 - bytes.count)) }
    }
    process.waitUntilExit()
    guard process.terminationStatus == 0 else {
        let message = String(decoding: bytes, as: UTF8.self).split(separator: "\n").filter { line in
            let event = (try? JSONSerialization.jsonObject(with: Data(line.utf8))) as? [String: Any]
            return event?["event"] as? String != "boot_error"
        }.joined(separator: "\n").trimmingCharacters(in: .whitespacesAndNewlines)
        throw APIError(message: message.isEmpty ? "升级操作未完成，请先核对切换状态。" : message)
    }
    return try JSONDecoder().decode(Result.self, from: bytes)
}
