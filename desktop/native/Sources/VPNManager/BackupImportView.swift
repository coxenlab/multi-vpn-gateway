import SwiftUI

struct SelectedBackup: Identifiable { let id = UUID(); let url: URL }

struct BackupImportView: View {
    let file: URL
    @EnvironmentObject var model: AppModel
    @Environment(\.dismiss) private var dismiss
    @StateObject private var state = BackupImportState()
    private var sessionID: ObjectIdentifier? { model.api.map(ObjectIdentifier.init) }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text(state.report == nil ? "导入配置备份" : "导入结果").font(.title2).bold()
            Text(file.lastPathComponent).foregroundStyle(.secondary).lineLimit(2)
            if state.loading { ProgressView("正在读取备份…") }
            if let error = state.error { Label(error, systemImage: "exclamationmark.triangle").foregroundStyle(.orange).textSelection(.enabled) }
            if let document = state.document {
                Text("备份包含 \(document.channelCount) 个通道条目、\(document.ruleCount) 条规则。")
                Text(document.names.joined(separator: "、") + (document.channelCount > 8 ? "…" : "")).foregroundStyle(.secondary).lineLimit(3)
                Text("导入会新增通道和规则，同名通道跳过；现有配置保留。导入的通道保持停止，之后可按需连接。")
                Text("格式错误或未知类型的条目会在结果中列出。备份中的自动登录凭据将存入本机配置。")
                    .font(.caption).foregroundStyle(.secondary)
                if document.channelCount == 0 { Text("这份备份没有通道，无需导入。").foregroundStyle(.secondary) }
            }
            if let report = state.report {
                Text("已新增 \(report.imported.count) 条通道，\(report.skipped.count) 个通道或规则条目未导入。")
                if report.deferred == true { Text("配置已保存，连接后生效。").foregroundStyle(.secondary) }
                else if report.pending { Label("配置已导入，规则同步仍待确认。请在分流规则中重试同步，无需重复导入。", systemImage: "exclamationmark.triangle").foregroundStyle(.orange) }
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 10) {
                        ForEach(Array(report.imported.enumerated()), id: \.offset) { _, name in Label(name, systemImage: "checkmark.circle").textSelection(.enabled) }
                        ForEach(Array(report.skipped.enumerated()), id: \.offset) { _, item in
                            VStack(alignment: .leading, spacing: 4) {
                                Text(item.name.isEmpty ? "未命名条目" : item.name).fontWeight(.medium)
                                Text(item.reason).font(.callout).foregroundStyle(.secondary)
                            }.textSelection(.enabled)
                        }
                    }.frame(maxWidth: .infinity, alignment: .leading)
                }.frame(maxHeight: .infinity)
            }
            Spacer(minLength: 0)
            HStack {
                if state.submitting { ProgressView("正在导入…").controlSize(.small) }
                Spacer()
                Button(state.report == nil ? "取消" : "完成") { state.cancel(); dismiss() }.keyboardShortcut(.cancelAction).disabled(state.submitting)
                if state.document != nil {
                    Button("导入通道与规则") { Task { await state.submit(model: model) } }.keyboardShortcut(.defaultAction)
                        .disabled(state.submitting || state.document?.channelCount == 0 || !model.ready)
                }
            }
        }.padding(24).frame(width: 580, height: 470)
            .task { await state.prepare(file: file, model: model) }
            .onChange(of: sessionID) { _, _ in state.connectionChanged() }
            .onDisappear { state.cancel() }
            .interactiveDismissDisabled(state.submitting)
    }
}
