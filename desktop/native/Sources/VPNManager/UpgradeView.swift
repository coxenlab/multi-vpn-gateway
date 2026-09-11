import SwiftUI

struct UpgradeReport: Decodable {
    let version: Int; let counts: [String: Int]; let encrypted_values_checked: Int; let files: [String]; let database_sha256: String; let runtime_verified: Bool; let ready_to_activate: Bool
}
struct UpgradeView: View {
    @EnvironmentObject var model: AppModel
    @State private var source: URL?
    @State private var parent: URL?
    var body: some View {
        Form {
            Section("准备升级副本") {
                Text("选择旧版本的数据目录和副本存放位置。检查密钥、通道、规则和恢复记录后，生成一份等待运行环境核对的副本。").foregroundStyle(.secondary)
                HStack { Text("原数据"); Spacer(); Text(source?.path ?? "尚未选择").lineLimit(2).textSelection(.enabled); Button("选择目录…") { source = chooseDirectory("选择旧版本的数据目录") } }
                HStack { Text("副本位置"); Spacer(); Text(parent?.path ?? "尚未选择").lineLimit(2).textSelection(.enabled); Button("选择位置…") { parent = chooseDirectory("选择用于存放升级副本的文件夹") } }
                Button("检查并准备副本") {
                    if let source, let parent {
                        let formatter = DateFormatter(); formatter.dateFormat = "yyyyMMdd-HHmmss"
                        let destination = parent.appendingPathComponent("vpnmgr-upgrade-\(formatter.string(from: Date()))-\(UUID().uuidString.prefix(6))", isDirectory: true)
                        Task { await model.prepareUpgrade(from: source, to: destination) }
                    }
                }.buttonStyle(.borderedProminent).disabled(source == nil || parent == nil || model.upgradePreparing)
            }.disabled(model.upgradePreparing)
            if model.upgradePreparing { ProgressView("正在检查数据并生成私有副本…") }
            if let error = model.upgradeError { Text(error).foregroundStyle(.red).textSelection(.enabled) }
            if let report = model.upgradeReport {
                Section("副本检查结果") {
                    LabeledContent("通道", value: String(report.counts["channels"] ?? 0))
                    LabeledContent("原分流规则", value: String(report.counts["rules"] ?? 0))
                    if let legacy = report.counts["domains"], legacy > 0 { LabeledContent("旧版域名记录", value: String(legacy)) }
                    LabeledContent("已核对加密字段", value: String(report.encrypted_values_checked))
                    LabeledContent("状态", value: "等待运行环境与切换验收")
                    Text("副本保留原有设备标识和卷引用。完成实例核对、切换与回退验证后才能启用。").font(.callout).foregroundStyle(.secondary)
                    DisclosureGroup("详细信息") {
                        Text(report.files.joined(separator: "、"))
                        Text(report.database_sha256).font(.system(.caption, design: .monospaced)).textSelection(.enabled)
                    }
                }
            }
            if let destination = model.upgradeDestination {
                Section("副本目录") {
                    Text(destination.path).textSelection(.enabled)
                    Button("在访达中查看") { NSWorkspace.shared.activateFileViewerSelecting([destination]) }
                    if model.upgradeReport == nil { Text("准备失败时保留的草稿不能作为运行数据目录使用，请按错误提示核对后重新准备。").font(.caption).foregroundStyle(.secondary) }
                }
            }
        }.formStyle(.grouped)
    }
    private func chooseDirectory(_ title: String) -> URL? {
        let panel = NSOpenPanel(); panel.title = title; panel.canChooseFiles = false; panel.canChooseDirectories = true; panel.allowsMultipleSelection = false
        return panel.runModal() == .OK ? panel.url : nil
    }
}

func runUpgradePreparation(executable: URL, environment: [String: String]?, source: URL, destination: URL) throws -> UpgradeReport {
    let process = Process(); process.executableURL = executable; process.environment = environment
    process.arguments = ["--prepare-upgrade", source.path, destination.path]
    process.standardInput = FileHandle.nullDevice
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
        throw APIError(message: message.isEmpty ? "副本准备未完成，请核对原目录与存放位置。" : message)
    }
    let report = try JSONDecoder().decode(UpgradeReport.self, from: bytes)
    guard report.version == 1, !report.runtime_verified, !report.ready_to_activate else { throw APIError(message: "升级报告状态不符合副本准备协议，请保留原目录并核对版本。") }
    return report
}
