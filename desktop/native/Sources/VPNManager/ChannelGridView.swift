import SwiftUI

struct ChannelGridView: View {
    @EnvironmentObject var model: AppModel
    @Binding var search: String
    let openChannel: (String) -> Void
    let createChannel: () -> Void
    private var channels: [Channel] {
        model.channels.filter { search.isEmpty || $0.name.localizedCaseInsensitiveContains(search) }
    }
    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 16) {
                HStack(alignment: .center) {
                    VStack(alignment: .leading, spacing: 5) {
                        Text("全部通道").font(.title2.bold())
                        Text(search.isEmpty ? "\(model.channels.count) 条通道" : "找到 \(channels.count) 条，共 \(model.channels.count) 条")
                            .font(.callout).foregroundStyle(.secondary)
                    }
                    Spacer()
                    Button(action: createChannel) { Label("新建通道", systemImage: "plus") }
                        .buttonStyle(.borderedProminent).help("新建通道（⌘N）")
                }
                TextField("搜索通道", text: $search).textFieldStyle(.roundedBorder).frame(maxWidth: 320)
            }.padding(24)
            Divider()
            if channels.isEmpty {
                ContentUnavailableView {
                    Label(model.channels.isEmpty ? "添加第一条通道" : "没有匹配的通道", systemImage: "network")
                } description: {
                    Text(model.channels.isEmpty ? "添加 VPN 通道后，在这里管理连接、登录和分流规则。" : "换个名称搜索，或清除搜索查看全部通道。")
                } actions: {
                    if model.channels.isEmpty { Button("新建通道", action: createChannel) }
                    else { Button("清除搜索") { search = "" } }
                }.frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                ScrollView {
                    LazyVGrid(columns: [GridItem(.adaptive(minimum: 270), spacing: 16)], alignment: .leading, spacing: 16) {
                        ForEach(channels) { channel in
                            ChannelCard(channel: channel) { openChannel(channel.id) }
                        }
                    }.padding(24).frame(maxWidth: .infinity, alignment: .topLeading)
                }.frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
            }
        }.frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
    }
}

private struct ChannelCard: View {
    @EnvironmentObject var model: AppModel
    let channel: Channel
    let open: () -> Void
    private var status: String { channel.statusLabel(runtime: model.system?.runtime) }
    private var type: String { model.adapters.first(where: { $0.key == channel.vpn_type })?.label ?? channel.vpn_type }
    private var statusColor: Color {
        if model.system?.runtime?.ready == false || channel.stop_pending == true { return .secondary }
        switch channel.status {
        case "logged_in": return .green
        case "error", "down": return .orange
        default: return .secondary
        }
    }
    var body: some View {
        Button(action: open) {
            VStack(alignment: .leading, spacing: 12) {
                HStack(alignment: .top, spacing: 10) {
                    Image(systemName: "network").font(.title3).foregroundStyle(.tint).accessibilityHidden(true)
                    Text(channel.name).font(.headline).lineLimit(2).multilineTextAlignment(.leading)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }.frame(height: 42, alignment: .top)
                Text(type).font(.callout).foregroundStyle(.secondary).lineLimit(1)
                Spacer(minLength: 8)
                HStack(spacing: 8) {
                    Circle().fill(statusColor).frame(width: 7, height: 7).accessibilityHidden(true)
                    Text(status).font(.callout).foregroundStyle(statusColor)
                    Spacer(minLength: 4)
                    Text("\(channel.rules.count) 条规则").font(.caption).foregroundStyle(.secondary)
                    Image(systemName: "chevron.right").font(.caption.weight(.semibold)).foregroundStyle(.tertiary).accessibilityHidden(true)
                }
            }.padding(20).frame(maxWidth: .infinity).frame(height: 166)
                .contentShape(RoundedRectangle(cornerRadius: 12))
        }.buttonStyle(ChannelCardStyle())
            .accessibilityLabel("\(channel.name)，\(type)，\(status)，\(channel.rules.count) 条规则")
            .accessibilityHint("打开通道详情")
            .help("打开“\(channel.name)”的连接、登录与分流规则")
    }
}

private struct ChannelCardStyle: ButtonStyle {
    @Environment(\.colorSchemeContrast) private var contrast
    @State private var hovering = false
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .background(Color(nsColor: .controlBackgroundColor), in: RoundedRectangle(cornerRadius: 12))
            .overlay {
                RoundedRectangle(cornerRadius: 12)
                    .fill(Color.accentColor.opacity(configuration.isPressed ? 0.10 : hovering ? 0.04 : 0))
                    .allowsHitTesting(false)
            }
            .overlay {
                RoundedRectangle(cornerRadius: 12)
                    .strokeBorder(hovering ? Color.accentColor : Color(nsColor: .separatorColor), lineWidth: contrast == .increased ? 2 : 1)
                    .allowsHitTesting(false)
            }
            .onHover { hovering = $0 }
    }
}
