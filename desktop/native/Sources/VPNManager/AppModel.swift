import AppKit
import SwiftUI

@MainActor final class AppModel: ObservableObject {
    @Published var channels: [Channel] = []
    @Published var system: SystemStatus?
    @Published var adapters: [Adapter] = []
    @Published var message: String?
    @Published var error: String?
    @Published var starting = false
    @Published var createdChannelID: String?
    @Published var busy = Set<String>()
    @Published var ready = false
    @Published var imageProgress: [String: String] = [:]
    @Published var importTicket: ImageImportTicket?
    @Published var importError: String?
    @Published var upgradePreparing = false
    @Published var upgradeReport: UpgradeReport?
    @Published var upgradeDestination: URL?
    @Published var upgradeError: String?
    @Published var visible = true
    private(set) var api: LocalAPI?
    private var child: Process?
    private var childInput: Pipe?
    private var childOutput: Pipe?
    private var output = Data()
    private var refreshTask: Task<Void,Never>?
    private var refreshJob: (id: UUID, client: LocalAPI, task: Task<Void, Never>)?
    @Published private(set) var quitting = false
    private var generation = UUID()
    private var noteDrafts: [String: NoteDraft] = [:]

    func noteDraft(for channelID: String) -> NoteDraft {
        if let draft = noteDrafts[channelID] { return draft }
        let draft = NoteDraft(); noteDrafts[channelID] = draft; return draft
    }

    // Only the owned process's ready/exit events change the active API connection.
    // Identity checks remain necessary even when cancellation arrives too late.
    func connect(to client: LocalAPI) {
        disconnect()
        api = client; ready = true; starting = false; error = nil
    }
    func disconnect() {
        if let api { Task { await api.invalidate() } }
        refreshJob?.task.cancel(); refreshJob = nil
        api = nil; ready = false; refreshTask?.cancel(); refreshTask = nil
        channels = []; system = nil; adapters = []; createdChannelID = nil
        busy.removeAll(); message = nil; imageProgress = [:]
        if importTicket != nil { importError = "本地服务连接已结束，上次导入结果待确认。请刷新镜像清单核对。" }
        importTicket = nil
    }
    func isCurrent(_ client: LocalAPI) -> Bool { ready && !quitting && api === client }

    func launch() {
        guard child == nil, !starting, !quitting else { return }
        starting = true; error = nil; output.removeAll(); generation = UUID()
        let currentGeneration = generation
        do {
            var env = ProcessInfo.processInfo.environment
            #if DEBUG
            guard env["VPNMGR_DEV_MODE"] == "1", let path = env["VPNMGR_CORE_PATH"], let directory = env["DATA_DIR"], !directory.isEmpty,
                  let profile = env["VPNMGR_VM_PROFILE"], !["vpnmgr", "default"].contains(profile) else {
                throw APIError(message: "请通过 native/dev.sh 启动隔离开发版。")
            }
            let executable = URL(fileURLWithPath: path)
            #else
            guard let executable = Bundle.main.url(forResource: "vpnmgr-core", withExtension: nil) else { throw APIError(message: "缺少本地运行组件，请重新安装完整版本。") }
            let directory = try FileManager.default.url(for: .applicationSupportDirectory, in: .userDomainMask, appropriateFor: nil, create: true).appendingPathComponent("com.vpnmgr.desktop")
            env["DATA_DIR"] = directory.path
            env["VPNMGR_VM_PROFILE"] = "vpnmgr"
            env.removeValue(forKey: "VPNMGR_DEV_MODE")
            #endif
            env["VPNMGR_MANAGED_VM"] = "1"; env["VPNMGR_NATIVE_CHILD"] = "1"
            let process = Process(); process.executableURL = executable; process.environment = env
            let input = Pipe(), pipe = Pipe(); process.standardInput = input; process.standardOutput = pipe
            // stderr stays with the local launcher; credentials are never copied into UI logs.
            process.terminationHandler = { [weak self] process in
                Task { @MainActor in
                    guard let self, self.generation == currentGeneration else { return }
                    self.generation = UUID() // Invalidate ready events already queued by this process.
                    self.child = nil; self.starting = false; self.disconnect()
                    self.childOutput?.fileHandleForReading.readabilityHandler = nil
                    try? self.childInput?.fileHandleForWriting.close()
                    self.childInput = nil; self.childOutput = nil
                    if self.quitting { NSApp.reply(toApplicationShouldTerminate: true) }
                    else if self.error == nil { self.error = "本地服务已退出（\(process.terminationStatus)），可以重试启动。" }
                }
            }
            pipe.fileHandleForReading.readabilityHandler = { [weak self] handle in
                let bytes = handle.availableData
                if bytes.isEmpty { handle.readabilityHandler = nil; return }
                Task { @MainActor in if self?.generation == currentGeneration { self?.read(bytes) } }
            }
            childInput = input; childOutput = pipe; child = process
            try process.run()
        } catch { child = nil; starting = false; self.error = error.localizedDescription }
    }
    private func read(_ data: Data) {
        output.append(data)
        while let lineEnd = output.firstIndex(of: 10) {
            let line = output.prefix(upTo: lineEnd); output.removeSubrange(...lineEnd)
            guard let item = (try? JSONSerialization.jsonObject(with: line)) as? [String: Any] else { continue }
            if item["event"] as? String == "boot_error" { error = item["message"] as? String ?? "本地服务未能启动"; starting = false; continue }
            guard item["event"] as? String == "ready", !ready, !quitting, child?.isRunning == true,
                  let port = item["ui_port"] as? Int, (1...65535).contains(port) else { continue }
            let client = LocalAPI(port: port); connect(to: client)
            Task { await self.refresh(); if self.isCurrent(client) { self.beginRefresh() } }
        }
        if output.count > 65536 { output.removeAll() }
    }
    func refresh() async {
        guard let api, isCurrent(api) else { return }
        // A write may finish while an older snapshot is loading. Wait for that snapshot,
        // then fetch once more; concurrent waiters share the newer refresh.
        if let previous = refreshJob, previous.client === api {
            await previous.task.value
            guard isCurrent(api), !Task.isCancelled else { return }
            if let next = refreshJob, next.id != previous.id, next.client === api {
                await next.task.value; return
            }
        }
        guard !Task.isCancelled else { return }
        let id = UUID(), task = Task { await self.loadSnapshot(from: api) }
        refreshJob = (id, api, task)
        await task.value
        if refreshJob?.id == id { refreshJob = nil }
    }
    private func loadSnapshot(from api: LocalAPI) async {
        do {
            async let list = api.get("/api/channels", as: [Channel].self)
            async let status = try? api.get("/api/system", as: SystemStatus.self)
            let nextChannels = try await list, nextSystem = await status
            guard isCurrent(api), !Task.isCancelled else { return }
            channels = nextChannels; system = nextSystem
            if error?.hasPrefix("刷新失败：") == true { error = nil }
            if adapters.isEmpty {
                let nextAdapters = (try? await api.get("/api/vpn-types", as: [Adapter].self)) ?? []
                if isCurrent(api), !Task.isCancelled { adapters = nextAdapters }
            }
        } catch { if isCurrent(api), !Task.isCancelled { self.error = "刷新失败：\(error.localizedDescription)" } }
    }
    private func beginRefresh() {
        refreshTask?.cancel()
        refreshTask = Task { [weak self] in
            while !Task.isCancelled {
                do { try await Task.sleep(for: .seconds(5)) } catch { return }
                guard let self else { return }
                if self.visible { await self.refresh() }
            }
        }
    }
    @discardableResult func perform(_ path: String, key: String, method: String = "POST", body: [String: Any]? = nil, success: String = "已完成") async -> Bool {
        guard let api, isCurrent(api), !busy.contains(key) else { return false }
        busy.insert(key); defer { if isCurrent(api) { busy.remove(key) } }
        error = nil
        do {
            let value = try await api.write(path, method: method, body: body)
            guard isCurrent(api) else { return false }
            message = try operationMessage(value, fallback: success)
            await refresh()
            guard isCurrent(api) else { return false }
            if path == "/api/channels", method == "POST" { createdChannelID = value["id"] as? String }
            if let rejected = value["rejected"] as? [String], !rejected.isEmpty {
                error = "部分规则未添加：" + rejected.joined(separator: "、")
                return false
            }
            if let imported = value["imported"] as? [Any], let skipped = value["skipped"] as? [Any] {
                message = "已导入 \(imported.count) 条通道，跳过 \(skipped.count) 条。"
            }
            return true
        } catch let failure as APIError where failure.saved {
            guard isCurrent(api) else { return false }
            self.error = "设置已保存，规则同步尚未确认。请在分流规则中重试同步，无需重复提交。"
            await refresh(); return false
        } catch {
            guard isCurrent(api) else { return false }
            self.error = "\(error.localizedDescription)\n若操作超时，请先刷新核对结果，再决定是否重试。"
            await refresh(); return false
        }
    }
    func downloadImage(_ image: ImageEntry) async {
        let key = "image-" + image.id
        guard let api, isCurrent(api), !busy.contains(key) else { return }
        busy.insert(key); defer { if isCurrent(api) { busy.remove(key) } }
        do {
            let result = try await api.write("/api/preflight/fix/pull_image", body: ["image": image.image])
            guard isCurrent(api) else { return }
            guard let id = result["task_id"] as? String else { throw APIError(message: "未收到下载任务，请刷新核对。") }
            while isCurrent(api) && !Task.isCancelled {
                if visible {
                    let state = try await api.get("/api/preflight/fix/\(id)", as: PullStatus.self)
                    guard isCurrent(api) else { return }
                    imageProgress[image.id] = state.progress
                    if state.status == "done" { message = "镜像已准备"; return }
                    if state.status == "error" { throw APIError(message: state.error ?? "下载未完成") }
                }
                try await Task.sleep(for: .seconds(2))
            }
        } catch { if isCurrent(api) { self.error = error.localizedDescription; imageProgress[image.id] = "下载状态待确认，请刷新核对" } }
    }
    func previewImage(_ file: URL) async {
        guard let api, isCurrent(api), importTicket == nil, !busy.contains("__image-import") else { return }
        busy.insert("__image-import"); importError = nil
        let scoped = file.startAccessingSecurityScopedResource()
        defer { if isCurrent(api) { busy.remove("__image-import") }; if scoped { file.stopAccessingSecurityScopedResource() } }
        do {
            let ticket = try await api.previewImage(file: file)
            if isCurrent(api) { importTicket = ticket }
        } catch { if isCurrent(api) { importError = error.localizedDescription } }
    }
    func confirmImageImport() async {
        guard let api, isCurrent(api), let ticket = importTicket, ticket.status == "preview", !busy.contains("__image-import") else { return }
        busy.insert("__image-import"); importError = nil; defer { if isCurrent(api) { busy.remove("__image-import") } }
        do {
            _ = try await api.write("/api/images/imports/\(ticket.id)/confirm")
            guard isCurrent(api) else { return }
            await refreshImageImport()
            while isCurrent(api) && !Task.isCancelled && importTicket?.id == ticket.id && importTicket?.status == "loading" {
                try await Task.sleep(for: .seconds(2))
                guard isCurrent(api) else { return }
                if visible { await refreshImageImport(); if importError != nil { return } }
            }
        } catch { if isCurrent(api) { importError = "\(error.localizedDescription) 请先刷新此任务状态，确认结果后再操作。" } }
    }
    func refreshImageImport() async {
        guard let api, isCurrent(api), let ticket = importTicket, !busy.contains("__image-refresh") else { return }
        busy.insert("__image-refresh"); defer { if isCurrent(api) { busy.remove("__image-refresh") } }
        do {
            let updated = try await api.get("/api/images/imports/\(ticket.id)", as: ImageImportTicket.self)
            if isCurrent(api), importTicket?.id == updated.id { importTicket = updated; importError = nil }
        } catch let error as APIError where error.statusCode == 404 {
            guard isCurrent(api), importTicket?.id == ticket.id else { return }
            importTicket = nil; importError = "此导入记录已失效。请先刷新镜像清单核对结果，需要时再重新选择文件。"
        } catch { if isCurrent(api), importTicket?.id == ticket.id { importError = error.localizedDescription } }
    }
    func discardImageImport() async {
        guard let api, isCurrent(api), let ticket = importTicket, ticket.status != "loading", !busy.contains("__image-import") else { return }
        busy.insert("__image-import"); defer { if isCurrent(api) { busy.remove("__image-import") } }
        do {
            _ = try await api.write("/api/images/imports/\(ticket.id)", method: "DELETE")
            guard isCurrent(api), importTicket?.id == ticket.id else { return }
            importTicket = nil; importError = nil
        } catch { if isCurrent(api) { importError = error.localizedDescription } }
    }
    func prepareUpgrade(from source: URL, to destination: URL) async {
        guard let executable = child?.executableURL, !upgradePreparing else { return }
        upgradePreparing = true; upgradeError = nil; upgradeReport = nil; upgradeDestination = destination
        let sourceScoped = source.startAccessingSecurityScopedResource(), destinationScoped = destination.deletingLastPathComponent().startAccessingSecurityScopedResource()
        defer {
            upgradePreparing = false
            if sourceScoped { source.stopAccessingSecurityScopedResource() }
            if destinationScoped { destination.deletingLastPathComponent().stopAccessingSecurityScopedResource() }
        }
        let environment = child?.environment
        do {
            upgradeReport = try await Task.detached(priority: .utility) { try runUpgradePreparation(executable: executable, environment: environment, source: source, destination: destination) }.value
        } catch { upgradeError = error.localizedDescription }
    }
    func uploadInstaller(_ file: URL, channelID: String) async {
        guard let api, isCurrent(api), !busy.contains(channelID) else { return }
        busy.insert(channelID)
        let scoped = file.startAccessingSecurityScopedResource()
        defer { if isCurrent(api) { busy.remove(channelID) }; if scoped { file.stopAccessingSecurityScopedResource() } }
        do {
            try await api.upload("/api/channels/\(channelID)/upload", file: file)
            if isCurrent(api) { message = "安装包已上传，请在登录桌面中继续安装。" }
        } catch { if isCurrent(api) { self.error = error.localizedDescription } }
    }
    func quit() -> NSApplication.TerminateReply {
        quitting = true; disconnect(); message = "正在断开并退出…"
        guard let child, child.isRunning else { return .terminateNow }
        try? childInput?.fileHandleForWriting.close() // EOF asks owned core to run its complete shutdown sequence.
        return .terminateLater
    }
}
