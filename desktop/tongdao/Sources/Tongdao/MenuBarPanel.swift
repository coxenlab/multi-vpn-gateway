import SwiftUI
import AppKit

/// Menu bar panel: online count, download speed, the routing switch and one row per channel.
/// Data comes from the model's /api/channels + /api/system snapshot; it refreshes every
/// 3 seconds, and samples /api/connections for the speed, only while the panel is open.
struct MenuBarPanel: View {
    @EnvironmentObject var model: AppModel
    @Environment(\.openWindow) private var openWindow
    @State private var speed: Double?
    @State private var sample: (total: Int64, at: Date)?
    @State private var working = false
    @State private var notice: String?

    static let brand = Color(red: 0x54 / 255, green: 0x57 / 255, blue: 0xE6 / 255)

    enum RowState { case stopped, failed, waiting, connected, unknown }
    /// Same order as the page's display states; the panel does not read /health.
    static func state(of channel: Channel) -> RowState {
        if channel.status == "stopped" || channel.stop_pending == true { return .stopped }
        if channel.container_state == "unknown" && ["running", "logged_in", "down"].contains(channel.status) { return .unknown }
        if ["error", "down"].contains(channel.status) { return .failed }
        if let phase = channel.replacement?.phase, !["awaiting_login", "committed", "rolled_back"].contains(phase) { return .failed }
        return channel.status == "logged_in" ? .connected : .waiting
    }
    /// The page's wording for the same states (app/ui/js/terms.js STATE).
    static let stateText: [RowState: String] = [.connected: "已连接", .waiting: "待登录", .failed: "出错", .stopped: "已停止", .unknown: "状态待确认"]

    enum RowAction: Equatable { case login(String), view(String), toggle(on: Bool) }
    /// What a row shows: the status line and its one action. Only channels that log in
    /// through the login window get 去登录; the page sends the others to their overview.
    static func row(_ channel: Channel) -> (text: String, action: RowAction) {
        let state = state(of: channel)
        let text = state == .connected ? channel.latency_ms.map { "已连接，\($0) 毫秒" } ?? "已连接" : stateText[state] ?? "待登录"
        switch state {
        case .waiting where channel.login_method != "headless": return (text, .login("#/channels/\(channel.id)?tab=login"))
        case .waiting, .failed, .unknown: return (text, .view("#/channels/\(channel.id)"))
        case .connected, .stopped: return (text, .toggle(on: state == .connected))
        }
    }
    static func onlineCount(_ channels: [Channel]) -> Int { channels.filter { state(of: $0) == .connected }.count }
    /// Number beside the menu bar icon; hidden until the local service answers.
    static func menuBarCount(ready: Bool, channels: [Channel], stale: Bool = false) -> String? {
        ready ? String(onlineCount(channels)) + (stale ? "·" : "") : nil
    }
    static func headerTitle(ready: Bool, channels: [Channel], stale: Bool) -> String {
        !ready ? "本地服务尚未就绪" : stale ? "状态待确认" : "\(onlineCount(channels)) 条通道在线"
    }

    /// The page's channels icon (three linked nodes), drawn as a template image.
    @MainActor static let icon: NSImage = {
        let image = NSImage(size: NSSize(width: 16, height: 16), flipped: true) { _ in
            let path = NSBezierPath()
            for (x, y) in [(3.5, 8.0), (12.5, 3.8), (12.5, 12.2)] { path.appendOval(in: NSRect(x: x - 1.8, y: y - 1.8, width: 3.6, height: 3.6)) }
            path.move(to: NSPoint(x: 5.2, y: 7.2)); path.line(to: NSPoint(x: 10.8, y: 4.6))
            path.move(to: NSPoint(x: 5.2, y: 8.8)); path.line(to: NSPoint(x: 10.8, y: 11.4))
            path.lineWidth = 1.35; path.lineCapStyle = .round; path.lineJoinStyle = .round
            NSColor.black.setStroke(); path.stroke()
            return true
        }
        image.isTemplate = true
        return image
    }()
    static func rate(_ bytesPerSecond: Double) -> String {
        let units = ["B", "KB", "MB", "GB"]
        var value = bytesPerSecond, index = 0
        while value >= 1024, index < units.count - 1 { value /= 1024; index += 1 }
        return (index == 0 ? String(Int(value)) : String(format: "%.1f", value)) + " \(units[index])/秒"
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            header
            if model.ready, !model.channels.isEmpty {
                if model.channels.count > 6 { ScrollView { rows }.frame(height: 6 * 48) } else { rows }
            }
            if let notice { Text(notice).font(.caption).foregroundStyle(.secondary).padding(.horizontal, 4) }
            VStack(spacing: 2) {
                item("打开主窗口", key: "⌘O") { show(nil) }.keyboardShortcut("o")
                item("退出", key: "⌘Q") { NSApp.terminate(nil) }.keyboardShortcut("q")
            }
        }.padding(14).frame(width: 330)
            .task { await poll() }
    }

    private var header: some View {
        HStack(spacing: 10) {
            Image(nsImage: Self.icon).renderingMode(.template).foregroundStyle(.white)
                .frame(width: 28, height: 28).background(Self.brand, in: RoundedRectangle(cornerRadius: 8))
            VStack(alignment: .leading, spacing: 1) {
                Text(Self.headerTitle(ready: model.ready, channels: model.channels, stale: model.snapshotStale))
                    .font(.system(size: 14, weight: .semibold))
                if let speed { Text("下载 \(Self.rate(speed))").font(.caption).foregroundStyle(.secondary).monospacedDigit() }
            }
            Spacer()
            Toggle("分流", isOn: Binding(get: { model.system?.routing_off != true }, set: { on in
                run { try await model.setRouting(off: !on) }
            })).labelsHidden().toggleStyle(.switch).disabled(!model.ready || model.system == nil || working)
        }.padding(.horizontal, 4).padding(.bottom, 4)
    }

    private var rows: some View {
        VStack(spacing: 0) {
            ForEach(model.channels) { channel in
                if channel.id != model.channels.first?.id { Divider() }
                row(channel)
            }
        }.padding(.horizontal, 12).background(.background, in: RoundedRectangle(cornerRadius: 14))
    }

    private func row(_ channel: Channel) -> some View {
        let row = Self.row(channel)
        return HStack(spacing: 10) {
            Text(String(channel.name.prefix(1))).font(.system(size: 13, weight: .semibold)).foregroundStyle(Self.brand)
                .frame(width: 28, height: 28).background(Self.brand.opacity(0.12), in: Circle())
            VStack(alignment: .leading, spacing: 1) {
                Text(channel.name).fontWeight(.semibold).lineLimit(1)
                Text(row.text).font(.caption).foregroundStyle(.secondary).monospacedDigit()
            }
            Spacer(minLength: 8)
            switch row.action {
            case .login(let hash):
                Button("去登录") { show(hash) }
                    .buttonStyle(.borderedProminent).tint(Self.brand).controlSize(.small)
            case .view(let hash):
                Button("查看") { show(hash) }.controlSize(.small)
            case .toggle(let on):
                Toggle(channel.name, isOn: Binding(get: { on }, set: { next in
                    run { try await model.setChannel(channel.id, running: next) }
                })).labelsHidden().toggleStyle(.switch).controlSize(.small).disabled(working)
            }
        }.frame(height: 48)
    }

    private func item(_ title: String, key: String, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            HStack { Text(title); Spacer(); Text(key).font(.caption).foregroundStyle(.tertiary) }
                .padding(.horizontal, 8).frame(height: 32).contentShape(Rectangle())
        }.buttonStyle(.plain)
    }

    private func show(_ hash: String?) {
        openWindow(id: "main"); NSApp.activate(ignoringOtherApps: true)
        if let hash { model.openInPage(hash) }
    }

    private func run(_ action: @escaping () async throws -> String) {
        working = true; notice = nil
        Task {
            do { notice = try await action() } catch { notice = error.localizedDescription }
            await model.refresh(); working = false
        }
    }

    private func poll() async {
        sample = nil; speed = nil
        while !Task.isCancelled {
            await model.refresh()
            if let total = await model.downloadTotal() {
                let now = Date()
                if let last = sample, now > last.at {
                    // Counters reset when the engine restarts; a negative delta counts as 0.
                    speed = Double(max(0, total - last.total)) / now.timeIntervalSince(last.at)
                }
                sample = (total, now)
            } else { speed = nil; sample = nil }
            do { try await Task.sleep(for: .seconds(3)) } catch { return }
        }
    }
}

/// Menu bar item: the channels icon and the online count, as in the prototype's menu bar.
struct MenuBarLabel: View {
    let count: String?
    var body: some View {
        HStack(spacing: 3) {
            Image(nsImage: MenuBarPanel.icon).renderingMode(.template)
            if let count { Text(count).monospacedDigit() }
        }.accessibilityLabel("通道管家")
    }
}
