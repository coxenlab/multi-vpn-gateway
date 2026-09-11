import SwiftUI

/// A gateway check covers the shared entry, not any channel's enterprise login or business target.
struct GatewayFeedback: Equatable {
    enum Tone { case quiet, good, warning, failure }
    enum Action { case refresh, repair, rules, settings, diagnose }
    let title: String
    let detail: String?
    let tone: Tone
    var action: Action = .diagnose
    var symbol: String {
        switch tone { case .quiet: "clock"; case .good: "checkmark.circle"; case .warning: "exclamationmark.triangle"; case .failure: "exclamationmark.circle" }
    }
    var color: Color {
        switch tone { case .quiet: .secondary; case .good: .green; case .warning: .orange; case .failure: .red }
    }
    static func read(_ system: SystemStatus?, error: String?, now: Date = .now) -> Self {
        if error != nil {
            return Self(title: "运行状态待确认", detail: "暂时无法读取运行状态，请刷新后核对。", tone: .warning, action: .refresh)
        }
        guard let system else { return Self(title: "正在读取运行状态", detail: nil, tone: .quiet) }
        if let runtime = system.runtime, !runtime.ready {
            return Self(title: runtime.label, detail: nil, tone: runtime.phase == "failed" ? .failure : .quiet)
        }
        guard let checked = system.gateway_checked_at_ms,
              (0...60).contains(now.timeIntervalSince1970 - Double(checked) / 1_000) else {
            return Self(title: "入口状态待确认", detail: "尚无近期检测结果。刷新可读取最新状态，旧结果不代表当前连接可用。", tone: .warning, action: .refresh)
        }
        let repairing = system.healing == true
        switch system.gateway_health {
        case "forward_dead", "transport_dead":
            if repairing {
                return Self(title: "正在修复分流连接", detail: "入口尚未通过检测，修复结果会在后续检测后更新。", tone: .warning)
            }
            if system.gave_up == true {
                return Self(title: "自动修复未能恢复连接", detail: "请查看运行诊断；也可以手动尝试修复入口。", tone: .failure, action: .repair)
            }
            let detail = system.self_heal_enabled == false ? "入口未通过检测，自动修复已暂停。可手动修复或查看运行诊断。" : "入口未通过检测，请查看运行诊断和自动修复状态。"
            return Self(title: "分流链路中断", detail: detail, tone: .failure, action: system.self_heal_enabled == false ? .repair : .diagnose)
        case "vm_down":
            return Self(title: "运行环境暂不可达", detail: "无法确认运行环境状态，请查看诊断。重新连接前请先保存当前工作。", tone: .failure)
        case "container_down":
            return Self(title: repairing ? "正在修复分流入口" : "分流入口已停止", detail: "入口尚未恢复，经本应用转发的连接可能受影响。", tone: .failure, action: repairing ? .diagnose : .repair)
        case "transport_degraded":
            return Self(title: "入口管理状态待确认", detail: "入口握手有响应，但无法读取运行环境状态。通道和业务连接仍需单独核对。", tone: .warning)
        case "healthy":
            guard system.proxy_port_reachable == true else {
                return Self(title: "入口状态待确认", detail: "入口检测结果不完整，请刷新后核对。", tone: .warning, action: .refresh)
            }
        default:
            return Self(title: "入口状态待确认", detail: "未取得可识别的检测结果，请刷新后核对。", tone: .warning, action: .refresh)
        }
        if repairing {
            return Self(title: "入口修复尚未结束", detail: "入口已有响应，正在等待修复结束和后续检测。", tone: .warning)
        }
        if system.vm_egress_dead == true {
            return Self(title: "运行环境出站检测失败", detail: "入口有响应，但连续出站检测失败。请查看诊断并核对实际业务连接。", tone: .warning)
        }
        if system.egress_guard_applied == false {
            return Self(title: "基础网络防护待确认", detail: "最近一次防护检查未能通过，请查看运行诊断。", tone: .warning)
        }
        guard system.mihomo_status == "running" else {
            return Self(title: "入口管理状态待确认", detail: "入口握手已通过，管理接口尚未恢复。规则同步可能受影响。", tone: .warning)
        }
        if system.config_application?.pending == true {
            return Self(title: "分流规则尚未同步", detail: "设置已保存，入口是否采用最新规则仍待确认。请在分流规则中核对或重试同步。", tone: .warning, action: .rules)
        }
        if system.routing_off == true {
            return Self(title: "分流已暂停", detail: "当前未启用通道分流，可在设置中恢复。", tone: .quiet, action: .settings)
        }
        return Self(title: "入口检测通过", detail: nil, tone: .good)
    }
}

struct GatewayStatusSummary: View {
    @EnvironmentObject var model: AppModel
    var body: some View {
        TimelineView(.periodic(from: .now, by: 5)) { context in
            let feedback = GatewayFeedback.read(model.system, error: model.systemReadError, now: context.date)
            Label(feedback.title, systemImage: feedback.symbol).foregroundStyle(feedback.color)
        }
    }
}

struct GatewayStatusView: View {
    @EnvironmentObject var model: AppModel
    let openPage: (Page) -> Void
    @State private var inspecting: Inspection?
    @State private var repairing = false
    var body: some View {
        TimelineView(.periodic(from: .now, by: 5)) { context in
            let feedback = GatewayFeedback.read(model.system, error: model.systemReadError, now: context.date)
            if let detail = feedback.detail {
                HStack(alignment: .top, spacing: 12) {
                    Image(systemName: feedback.symbol).foregroundStyle(feedback.color).accessibilityHidden(true)
                    VStack(alignment: .leading, spacing: 6) {
                        Text(feedback.title).font(.headline)
                        Text(detail).font(.callout).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                        HStack {
                            switch feedback.action {
                            case .refresh: Button("刷新状态") { Task { await model.refresh() } }
                            case .repair: Button("修复入口…") { repairing = true }.disabled(model.busy.contains("__heal"))
                            case .rules: Button("查看分流规则") { openPage(.rules) }
                            case .settings: Button("打开设置") { openPage(.settings) }
                            case .diagnose: EmptyView()
                            }
                            Button("运行诊断") { inspecting = Inspection(path: "/api/diag", title: "运行诊断") }
                            Button("运行事件") { inspecting = Inspection(path: "/api/events", title: "运行事件") }
                        }.controlSize(.small)
                    }
                    Spacer(minLength: 0)
                }.padding(12).frame(maxWidth: .infinity, alignment: .leading).background(.quaternary)
                    .accessibilityElement(children: .contain)
            }
        }
        .sheet(item: $inspecting) { item in
            VStack {
                if item.path == "/api/events" { EventsView() }
                else { TextEndpointView(path: item.path, title: item.title) }
                Button("关闭") { inspecting = nil }.keyboardShortcut(.cancelAction).padding()
            }.frame(width: 780, height: 620)
        }
        .confirmationDialog("修复分流入口？", isPresented: $repairing, titleVisibility: .visible) {
            Button("修复入口") { Task { await model.repairGateway() } }
        } message: { Text("将按当前配置修复入口服务，可能短暂影响经本应用转发的连接。") }
    }
}
