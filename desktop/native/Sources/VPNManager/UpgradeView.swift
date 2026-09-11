import SwiftUI

struct RuleCleanupReport: Decodable {
    let version: Int
    let examined: Int
    let normalized: Int
    let quarantined: Int
}

struct UpgradeReport: Decodable {
    let version: Int; let counts: [String: Int]; let encrypted_values_checked: Int; let files: [String]; let database_sha256: String; let runtime_verified: Bool; let ready_to_activate: Bool
    let vm_profile: String?
    let source: String?
    let rule_cleanup: RuleCleanupReport?
}
struct UpgradeView: View {
    @EnvironmentObject var model: AppModel
    @State private var source: URL?
    @State private var parent: URL?
    @State private var action: UpgradeAction?
    private var working: Bool { model.upgradePreparing || model.upgradeSwitching }
    var body: some View {
        Form {
            Section("准备升级副本") {
                Text("先从旧版本断开并退出，再选择原数据目录。检查通道、规则与加密信息，生成可核对的升级副本。").foregroundStyle(.secondary)
                HStack { Text("原数据"); Spacer(); Text(source?.path ?? "尚未选择").lineLimit(2).textSelection(.enabled); Button("选择目录…") { if let selected = chooseDirectory("选择旧版本的数据目录") { source = selected; model.upgradeReport = nil } } }
                HStack { Text("副本位置"); Spacer(); Text(parent?.path ?? "尚未选择").lineLimit(2).textSelection(.enabled); Button("选择位置…") { if let selected = chooseDirectory("选择用于存放升级副本的文件夹") { parent = selected; model.upgradeReport = nil } } }
                Button("检查并准备副本") {
                    if let source, let parent {
                        let formatter = DateFormatter(); formatter.dateFormat = "yyyyMMdd-HHmmss"
                        let destination = parent.appendingPathComponent("vpnmgr-upgrade-\(formatter.string(from: Date()))-\(UUID().uuidString.prefix(6))", isDirectory: true)
                        Task { await model.prepareUpgrade(from: source, to: destination) }
                    }
                }.buttonStyle(.borderedProminent).disabled(source == nil || parent == nil || !model.canManageUpgrade)
            }.disabled(working || model.upgradeStatus != nil)
            if model.upgradePreparing { ProgressView("正在检查数据并生成私有副本…") }
            if model.upgradeSwitching { ProgressView("正在退出本地服务并处理配置切换…") }
            if let error = model.upgradeError { Text(error).foregroundStyle(.red).textSelection(.enabled) }
            if let report = model.upgradeReport {
                Section("副本检查结果") {
                    LabeledContent("通道", value: String(report.counts["channels"] ?? 0))
                    LabeledContent("原分流规则", value: String(report.counts["rules"] ?? 0))
                    if let legacy = report.counts["domains"], legacy > 0 { LabeledContent("旧版域名记录", value: String(legacy)) }
                    LabeledContent("已核对加密字段", value: String(report.encrypted_values_checked))
                    if let cleanup = report.rule_cleanup {
                        LabeledContent("历史规则格式整理", value: "\(cleanup.normalized) 条")
                        LabeledContent("历史无效规则隔离", value: "\(cleanup.quarantined) 条")
                        if cleanup.normalized + cleanup.quarantined > 0 {
                            Text("处理过的原始记录已保留在副本中。隔离规则不参与分流；可在原目录核对内容后，重新添加有效规则。").font(.callout).foregroundStyle(.secondary)
                        }
                    } else {
                        Text("这份旧副本没有历史规则整理记录，可重新准备副本以核对。").font(.callout).foregroundStyle(.secondary)
                    }
                    LabeledContent("状态", value: "副本已核对，尚未启用")
                    if let original = report.source { LabeledContent("原数据", value: original).textSelection(.enabled) }
                    if let target = model.upgradeTargetPath { LabeledContent("启用位置", value: target).textSelection(.enabled) }
                    Text("启用只切换当前应用的配置。原目录保留，设备标识与数据卷引用沿用；不会因此确认 VPN 已登录。").font(.callout).foregroundStyle(.secondary)
                    Button("启用这个副本…") { action = .activate }.disabled(working || report.version != 2 || model.upgradeStatus != nil)
                    DisclosureGroup("详细信息") {
                        Text(report.files.joined(separator: "、"))
                        Text(report.database_sha256).font(.system(.caption, design: .monospaced)).textSelection(.enabled)
                    }
                }
            }
            Section("配置切换与恢复") {
                Button("核对当前切换状态") { Task { await model.loadUpgradeStatus() } }.disabled(working || !model.canManageUpgrade)
                if let status = model.upgradeStatus {
                    LabeledContent("状态", value: status.label)
                    if status.phase == "active" { Text("请逐条连接并确认内网可达，再完成升级记录。").foregroundStyle(.secondary) }
                    Text(status.target).font(.caption).textSelection(.enabled)
                    HStack {
                        if status.canResume { Button("恢复切换…") { action = .resume } }
                        if status.canRollback { Button("回退配置…", role: .destructive) { action = .rollback } }
                        if ["active", "rolled_back"].contains(status.phase) { Button("完成升级记录…") { action = .finish } }
                    }.disabled(working)
                    Button("在访达中查看保留的数据") { NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: status.retained_directory)]) }
                    Text("保留目录不会自动删除。回退只恢复切换前的配置，不会撤销客户端内的文件或账号变化。").font(.caption).foregroundStyle(.secondary)
                } else {
                    Text("没有待处理的切换记录。").foregroundStyle(.secondary)
                    if let completed = model.completedUpgrade {
                        Button("查看已归档的保留数据") { NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: completed.retained_directory)]) }
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
        }.formStyle(.grouped).task { await model.loadUpgradeStatus() }
            .confirmationDialog(action?.title ?? "配置切换", isPresented: Binding(get: { action != nil }, set: { if !$0 { action = nil } }), titleVisibility: .visible) {
                if let selected = action { Button(selected.title, role: selected == .rollback ? .destructive : nil) { action = nil; Task { await model.switchUpgrade(selected) } } }
            } message: {
                Text("将先断开当前应用并关闭本地服务，然后处理配置并重新启动管理界面。原目录与保留的数据不会删除；启用或回退前，原运行环境必须已停止。")
            }
    }
    private func chooseDirectory(_ title: String) -> URL? {
        let panel = NSOpenPanel(); panel.title = title; panel.canChooseFiles = false; panel.canChooseDirectories = true; panel.allowsMultipleSelection = false
        return panel.runModal() == .OK ? panel.url : nil
    }
}

func runUpgradePreparation(executable: URL, environment: [String: String]?, source: URL, destination: URL) throws -> UpgradeReport {
    let report = try runOfflineCore(context: OfflineLaunchContext(executable: executable, environment: environment ?? ProcessInfo.processInfo.environment), arguments: ["--prepare-upgrade", source.path, destination.path], as: UpgradeReport.self)
    guard [1, 2].contains(report.version), !report.runtime_verified, !report.ready_to_activate else { throw APIError(message: "升级报告状态不符合副本准备协议，请保留原目录并核对版本。") }
    return report
}
