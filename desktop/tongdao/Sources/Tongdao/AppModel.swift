import AppKit
import SwiftUI

@MainActor final class AppModel: ObservableObject {
    @Published var channels: [Channel] = []
    @Published var system: SystemStatus?
    @Published private(set) var systemReadError: String?
    @Published private(set) var channelsReadError: String?
    var snapshotStale: Bool { channelsReadError != nil || systemReadError != nil }
    @Published var adapters: [Adapter] = []
    @Published var message: String?
    @Published var error: String?
    @Published var starting = false
    @Published var ready = false
    @Published var visible = true
    private(set) var api: LocalAPI?
    private(set) var dataDirectory: URL?
    weak var window: NSWindow?
    private var child: Process?
    private var childInput: Pipe?
    private var childOutput: Pipe?
    private var output = Data()
    private var refreshTask: Task<Void,Never>?
    private var refreshJob: (id: UUID, client: LocalAPI, task: Task<Void, Never>)?
    @Published private(set) var quitting = false
    private var generation = UUID()
    private let launchEnvironment: [String: String]?
    private let automaticRefresh: Bool
    private let refreshInterval: Duration

    init(launchEnvironment: [String: String]? = nil, automaticRefresh: Bool = true, refreshInterval: Duration = .seconds(30)) {
        self.launchEnvironment = launchEnvironment
        self.automaticRefresh = automaticRefresh
        self.refreshInterval = refreshInterval
    }

    // Only the owned process's ready/exit events change the active API connection.
    // Identity checks remain necessary even when cancellation arrives too late.
    func connect(to client: LocalAPI) {
        disconnect()
        api = client; ready = true; starting = false; error = nil
        if automaticRefresh { beginRefresh(client) }
    }
    func disconnect() {
        if let api { Task { await api.invalidate() } }
        refreshJob?.task.cancel(); refreshJob = nil
        api = nil; ready = false; refreshTask?.cancel(); refreshTask = nil
        channels = []; system = nil; systemReadError = nil; channelsReadError = nil; adapters = []
        message = nil
    }
    func isCurrent(_ client: LocalAPI) -> Bool { ready && !quitting && api === client }

    func launch() {
        guard child == nil, !starting, !quitting else { return }
        starting = true; error = nil; output.removeAll(); generation = UUID()
        let currentGeneration = generation
        do {
            var env = launchEnvironment ?? ProcessInfo.processInfo.environment
            #if DEBUG
            // Development builds only run an explicit tongdao-* profile; the installed
            // tongdao profile and every vpnmgr profile are never reachable from here.
            guard env["VPNMGR_DEV_MODE"] == "1", let path = env["VPNMGR_CORE_PATH"], let directory = env["DATA_DIR"], !directory.isEmpty,
                  let profile = env["VPNMGR_VM_PROFILE"], profile.hasPrefix("tongdao-") else {
                throw APIError(message: "请通过 desktop/tongdao/dev.sh 启动隔离开发版。")
            }
            let executable = URL(fileURLWithPath: path)
            #else
            let otherInstance = Bundle.main.bundleIdentifier.map { identifier in
                NSRunningApplication.runningApplications(withBundleIdentifier: identifier)
                    .contains { $0.processIdentifier != ProcessInfo.processInfo.processIdentifier && !$0.isTerminated }
            } ?? false
            let plan = try NativeLaunchPlan.production(resources: Bundle.main.resourceURL,
                home: FileManager.default.homeDirectoryForCurrentUser, inheritedEnvironment: env, otherInstanceRunning: otherInstance)
            let executable = plan.executable
            env = plan.environment
            #endif
            dataDirectory = env["DATA_DIR"].flatMap { $0.isEmpty ? nil : URL(fileURLWithPath: $0) }
            env["VPNMGR_MANAGED_VM"] = "1"; env["VPNMGR_NATIVE_CHILD"] = "1"
            env = NativeLaunchPlan.withoutProxyVariables(env)
            let process = Process(); process.executableURL = executable; process.environment = env
            #if !DEBUG
            process.currentDirectoryURL = plan.workingDirectory
            #endif
            let input = Pipe(), pipe = Pipe(); process.standardInput = input; process.standardOutput = pipe
            // A desktop launcher may close its stderr pipe. Persistent, redacted events
            // are handled by core; diagnostics must not depend on the launcher's lifetime.
            #if !DEBUG
            process.standardError = FileHandle.nullDevice
            #endif
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
        } catch {
            generation = UUID()
            childOutput?.fileHandleForReading.readabilityHandler = nil
            try? childInput?.fileHandleForWriting.close()
            childInput = nil; childOutput = nil; child = nil
            starting = false; self.error = error.localizedDescription
        }
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
            Task { await self.refresh() }
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
        async let list: Result<[Channel], Error> = {
            do { return .success(try await api.get("/api/channels", as: [Channel].self)) }
            catch { return .failure(error) }
        }()
        async let status: Result<SystemStatus, Error> = {
            do { return .success(try await api.get("/api/system", as: SystemStatus.self)) }
            catch { return .failure(error) }
        }()
        let (nextChannels, nextSystem) = await (list, status)
        guard isCurrent(api), !Task.isCancelled else { return }
        switch nextSystem {
        case .success(let value): system = value; systemReadError = nil
        case .failure(let error): system = nil; systemReadError = error.localizedDescription
        }
        switch nextChannels {
        case .success(let value):
            channels = value
            channelsReadError = nil
            if error?.hasPrefix("刷新失败：") == true { error = nil }
            if adapters.isEmpty {
                let nextAdapters = (try? await api.get("/api/vpn-types", as: [Adapter].self)) ?? []
                if isCurrent(api), !Task.isCancelled { adapters = nextAdapters }
            }
        case .failure(let error):
            channelsReadError = error.localizedDescription
            self.error = "刷新失败：\(error.localizedDescription)"
        }
    }
    private func beginRefresh(_ client: LocalAPI) {
        refreshTask?.cancel()
        let interval = refreshInterval
        refreshTask = Task { [weak self] in
            while !Task.isCancelled {
                do { try await Task.sleep(for: interval) } catch { return }
                guard let self, self.isCurrent(client) else { return }
                await self.refresh()
            }
        }
    }
    // Menu bar panel writes: only the routing switch and channel start/stop.
    func setRouting(off: Bool) async throws -> String {
        guard let api, isCurrent(api) else { throw APIError(message: "本地服务尚未就绪") }
        return try operationMessage(await api.write("/api/routing", body: ["off": off]), fallback: off ? "分流已关闭" : "分流已开启")
    }
    func setChannel(_ id: String, running: Bool) async throws -> String {
        guard let api, isCurrent(api) else { throw APIError(message: "本地服务尚未就绪") }
        return try operationMessage(await api.write("/api/channels/\(id)/\(running ? "start" : "stop")"), fallback: running ? "已启动" : "已停止")
    }
    /// Cumulative download bytes; the panel derives its speed from successive samples.
    func downloadTotal() async -> Int64? {
        guard let api, isCurrent(api), let bytes = try? await api.data("/api/connections"),
              let value = (try? JSONSerialization.jsonObject(with: bytes)) as? [String: Any] else { return nil }
        return (value["downloadTotal"] as? NSNumber)?.int64Value
    }
    /// Routes the main window's page to `hash`. The page controller holds the jump until its
    /// page has loaded; before the page is mounted the jump waits here for the next page.
    private var pendingPageHash: String?
    func openInPage(_ hash: String) {
        if let controller = window?.workspaceController { controller.open(hash) } else { pendingPageHash = hash }
    }
    func takePendingPageHash() -> String? {
        defer { pendingPageHash = nil }
        return pendingPageHash
    }

    private struct AppSettings: Decodable { let keep_running_on_close: Bool? }
    /// Reads keep_running_on_close from core's app-settings.json. A missing or unreadable
    /// file, or a missing value, keeps the window-hiding behaviour (true).
    nonisolated static func keepRunningOnClose(dataDirectory: URL?) -> Bool {
        guard let url = dataDirectory?.appendingPathComponent("app-settings.json"),
              let data = try? Data(contentsOf: url),
              let settings = try? JSONDecoder().decode(AppSettings.self, from: data) else { return true }
        return settings.keep_running_on_close ?? true
    }

    func quit() -> NSApplication.TerminateReply {
        quitting = true; disconnect(); message = "正在断开并退出…"
        guard let child, child.isRunning else { return .terminateNow }
        try? childInput?.fileHandleForWriting.close() // EOF asks owned core to run its complete shutdown sequence.
        return .terminateLater
    }
}
