import SwiftUI

struct RuntimeStatusView: View {
    @EnvironmentObject var model: AppModel
    var allowsConnection = true
    private var runtime: Runtime? { model.system?.runtime }
    private var pending: Bool { model.busy.contains("__runtime") || runtime?.working == true }
    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                if pending { ProgressView().controlSize(.small) }
                Text(runtime?.label ?? "正在核对运行环境")
                Spacer()
                if allowsConnection, runtime?.ready != true {
                    Button(runtime?.phase == "failed" ? "重试连接" : "连接运行环境") {
                        Task { await model.perform("/api/runtime/start", key: "__runtime", success: "运行环境已连接") }
                    }.disabled(!model.ready || pending || runtime?.canConnect != true)
                }
            }
            if let notice = runtime?.notice {
                Text(notice).font(.callout).foregroundStyle(runtime?.phase == "failed" ? .red : .secondary).textSelection(.enabled)
            }
            if runtime?.quietPreparation == true {
                Text("已有一段时间没有新的进度信息，任务可能仍在进行。可在设置的运行事件中查看详情，或确认是否有待处理的系统授权。")
                    .font(.caption).foregroundStyle(.secondary)
            }
        }.accessibilityElement(children: .contain)
    }
}
