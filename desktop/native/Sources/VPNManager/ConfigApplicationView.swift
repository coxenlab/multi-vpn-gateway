import SwiftUI

struct ConfigApplication: Decodable {
    let available: Bool
    let pending: Bool
    let verified_at: Int64?
    let last_error: String?
    func confirmed(in system: SystemStatus) -> Bool {
        available && !pending && system.mihomo_status == "running" && (system.runtime == nil || system.runtime?.ready == true)
    }
    func title(in system: SystemStatus) -> String {
        if system.runtime?.phase == "dormant" { return pending ? "设置已保存，连接后生效" : "运行环境按需启用" }
        if !available { return "规则同步状态待确认" }
        if system.mihomo_status != "running" { return "分流入口离线，规则同步待确认" }
        return confirmed(in: system) ? "规则已确认同步" : "设置已保存，规则同步尚未完成"
    }
    var reason: String? {
        ["unconfirmed":"同步尚未完成。", "reload_failed":"同步响应尚未确认。", "readback_failed":"暂时无法读取运行规则。",
         "dns_flush_failed":"通道地址缓存尚未确认刷新。", "readback_mismatch":"运行规则与已保存设置还不一致。",
         "write_failed":"运行规则已读回，启动配置保存失败。", "delivery_failed":"运行规则已读回，启动配置投递尚未确认。",
         "state_unavailable":"暂时无法读取同步记录。", "runtime_idle":"连接运行环境后会同步这些设置。"][last_error ?? ""]
    }
}

/// An accepted HTTP response can still describe a failed or partly applied operation.
func operationMessage(_ value: [String: Any], fallback: String) throws -> String {
    if value["ok"] as? Bool == false { throw APIError(message: value["error"] as? String ?? "操作未完成，请刷新核对状态。", saved: value["saved"] as? Bool == true) }
    if value["stop_pending"] as? Bool == true { return "已保存停用，实际停止尚待确认" }
    if value["deferred"] as? Bool == true { return "已保存，连接后应用" }
    if value["rules_applied"] as? Bool == false || (value["config_application"] as? [String: Any])?["pending"] as? Bool == true { return "设置已保存，规则同步尚未确认" }
    return fallback
}

struct ConfigApplicationView: View {
    @EnvironmentObject var model: AppModel
    private var busy: Bool { model.busy.contains("__sync") || model.busy.contains("__runtime") || ["starting", "releasing"].contains(model.system?.runtime?.phase ?? "") }
    var body: some View {
        if let system = model.system, let state = system.config_application {
            VStack(alignment: .leading, spacing: 6) {
                HStack {
                    Label(state.title(in: system), systemImage: state.confirmed(in: system) ? "checkmark.circle" : "clock")
                    Spacer()
                    Button(busy ? "处理中…" : system.runtime?.ready == false ? "连接并同步" : state.pending ? "重试同步" : "重新核对") {
                        Task { await model.perform("/api/config/retry", key: "__sync", success: "规则已确认同步") }
                    }.disabled(busy)
                }
                if let reason = state.reason { Text(reason).font(.caption).foregroundStyle(.secondary) }
                if let checked = state.verified_at {
                    Text("上次确认同步：\(Date(timeIntervalSince1970: TimeInterval(checked)).formatted(date: .abbreviated, time: .shortened))。数量按已保存设置计算。")
                        .font(.caption).foregroundStyle(.secondary)
                } else { Text("尚无成功同步记录，数量按已保存设置计算。").font(.caption).foregroundStyle(.secondary) }
            }.padding(12).background(.quaternary, in: RoundedRectangle(cornerRadius: 8))
        }
    }
}
