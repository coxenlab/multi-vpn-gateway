import SwiftUI
import WebKit
import AppKit

/// The web pages use the owned core's HTTP API directly.
/// Reloading or losing the renderer never restarts the core or the VPN runtime.
struct WebWorkspaceView: View {
    @EnvironmentObject var model: AppModel
    var body: some View {
        VStack(spacing: 0) {
            if model.ready, let client = model.api {
                WebWorkspaceContent(client: client).id(ObjectIdentifier(client))
            } else {
                ContentUnavailableView {
                    Label(model.quitting ? "正在断开并退出" : model.starting ? "正在准备管理界面" : "本地服务尚未就绪", systemImage: "network")
                } description: {
                    Text(model.error ?? "管理界面就绪后，连接通道时才会准备运行环境。")
                } actions: {
                    if model.starting || model.quitting { ProgressView() }
                    else { Button("重试") { model.launch() } }
                }
            }
        }.frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

private struct WebWorkspaceContent: View {
    @EnvironmentObject var model: AppModel
    let client: LocalAPI
    @StateObject private var controller = WebWorkspaceController()
    var body: some View {
        VStack(spacing: 0) {
            if let error = controller.error {
                HStack {
                    Label(error, systemImage: "exclamationmark.triangle")
                    Spacer()
                    Button("重新载入页面") { controller.reload() }
                }.font(.callout).padding(12)
            }
            WorkspaceWebView(controller: controller)
        }.task {
            let url = await client.url("/")
            guard !Task.isCancelled, model.isCurrent(client) else { return }
            controller.connect(url, visible: model.visible)
            if let hash = model.takePendingPageHash() { controller.open(hash) }
        }.onChange(of: model.visible) { _, visible in controller.setVisible(visible) }
            .focusedSceneValue(\.workspaceActions, WorkspaceActions(create: {
                controller.open("?sheet=add")
            }, settings: {
                controller.open("#/settings")
            }))
            .pageRefresh { controller.reload() }
    }
}

private struct WorkspaceWebView: NSViewRepresentable {
    @ObservedObject var controller: WebWorkspaceController
    func makeNSView(context: Context) -> WKWebView { controller.webView }
    func updateNSView(_ view: WKWebView, context: Context) {}
    static func dismantleNSView(_ view: WKWebView, coordinator: ()) {
        view.stopLoading(); view.navigationDelegate = nil; view.uiDelegate = nil
        view.loadHTMLString("", baseURL: nil)
    }
}

/// Jumps asked for while the page cannot take them: before the first load finishes,
/// during a reload, after the renderer exits, or when the page has no `__tongdaoOpen`.
/// They run in order when the next load finishes, each exactly once.
struct PendingJumps {
    private(set) var waiting: [String] = []
    private(set) var loading = true
    private var failures: [String: Int] = [:]
    /// The hash to send now, or nil when it has to wait for the page.
    mutating func request(_ hash: String) -> String? {
        guard loading else { return hash }
        waiting.append(hash); return nil
    }
    mutating func started() { loading = true }
    /// The page finished loading: every waiting jump, handed out once.
    mutating func finished() -> [String] {
        loading = false
        defer { waiting = [] }
        return waiting
    }
    /// The page did not take `hash`; it waits for the next finished load.
    @discardableResult mutating func undelivered(_ hash: String) -> Bool {
        let count = (failures[hash] ?? 0) + 1
        failures[hash] = count
        if count == 1 { if !waiting.contains(hash) { waiting.append(hash) } }
        else { waiting.removeAll { $0 == hash } }
        return count == 2
    }
    mutating func delivered(_ hash: String) {
        failures.removeValue(forKey: hash)
        waiting.removeAll { $0 == hash }
    }
}

@MainActor final class WebWorkspaceController: NSObject, ObservableObject, WKNavigationDelegate, WKUIDelegate, WKDownloadDelegate {
    @Published private(set) var error: String?
    private(set) var origin: URL?
    private var visible = true
    private var jumps = PendingJumps()
    private var downloads: [ObjectIdentifier: WKDownload] = [:]
    let webView: WKWebView

    // AppKit window hiding/occlusion is propagated without unloading the page:
    // draft inputs survive and page polling / noVNC leases suspend normally.
    static let visibilityScript = """
    (() => {
      const hidden = Object.getOwnPropertyDescriptor(Document.prototype, 'hidden').get;
      const state = Object.getOwnPropertyDescriptor(Document.prototype, 'visibilityState').get;
      let visible = true;
      Object.defineProperty(document, 'hidden', {get: () => !visible || hidden.call(document)});
      Object.defineProperty(document, 'visibilityState', {get: () => !visible ? 'hidden' : state.call(document)});
      window.__tongdaoSetVisible = value => {
        const before = document.hidden;
        visible = value === true;
        if (before !== document.hidden) document.dispatchEvent(new Event('visibilitychange'));
      };
    })();
    """

    /// Installed builds show their bundle version; `swift run` has no Info.plist and shows 开发版.
    nonisolated static var bundleVersion: String {
        (Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String).flatMap { $0.isEmpty ? nil : $0 } ?? "开发版"
    }
    nonisolated static func versionScript(_ version: String) -> String { "window.__tongdaoAppVersion = \(jsString(version));" }
    nonisolated static func jsString(_ value: String) -> String {
        let data = (try? JSONSerialization.data(withJSONObject: [value])) ?? Data("[\"\"]".utf8)
        return String(decoding: data.dropFirst().dropLast(), as: UTF8.self)
    }

    init(appVersion: String = WebWorkspaceController.bundleVersion) {
        let config = WKWebViewConfiguration()
        config.websiteDataStore = .nonPersistent()
        config.userContentController.addUserScript(WKUserScript(source: Self.visibilityScript, injectionTime: .atDocumentStart, forMainFrameOnly: true))
        config.userContentController.addUserScript(WKUserScript(source: Self.versionScript(appVersion), injectionTime: .atDocumentStart, forMainFrameOnly: true))
        webView = WKWebView(frame: .zero, configuration: config)
        super.init()
        webView.navigationDelegate = self; webView.uiDelegate = self
        webView.allowsBackForwardNavigationGestures = true
    }
    func connect(_ url: URL, visible: Bool) {
        guard url.scheme == "http", url.host == "127.0.0.1", url.port != nil, origin == nil else { return }
        origin = url; self.visible = visible
        jumps.started()
        webView.load(URLRequest(url: url))
    }
    /// Menu actions route inside the loaded page; the page's main.js owns `__tongdaoOpen`.
    /// Until the page can take the jump it waits, and runs once the page has loaded.
    func open(_ hash: String) {
        guard let hash = jumps.request(hash) else { return }
        send(hash)
    }
    private func send(_ hash: String) {
        guard let url = webView.url, isLocal(url) else { undelivered(hash); return }
        let script = "typeof window.__tongdaoOpen === 'function' ? (window.__tongdaoOpen(\(Self.jsString(hash))), true) : false"
        webView.evaluateJavaScript(script) { [weak self] result, _ in
            if result as? Bool == true { self?.jumps.delivered(hash) }
            else { self?.undelivered(hash) }
        }
    }
    private func undelivered(_ hash: String) {
        guard jumps.undelivered(hash), let origin,
              let destination = Self.fallbackURL(origin: origin, current: webView.url, target: hash) else { return }
        jumps.started()
        webView.load(URLRequest(url: destination))
    }
    nonisolated static func fallbackURL(origin: URL, current: URL?, target: String) -> URL? {
        var parts = URLComponents(url: origin, resolvingAgainstBaseURL: false)
        if target.hasPrefix("#/") { parts?.fragment = String(target.dropFirst()) }
        else if target.hasPrefix("?") {
            let local = current?.scheme == origin.scheme && current?.host == origin.host && current?.port == origin.port
            var route = URLComponents(string: (local ? current?.fragment : nil) ?? "/home")
            let patch = URLComponents(string: target)?.queryItems ?? []
            let keys = Set(patch.map(\.name))
            let retained = (route?.queryItems ?? []).filter { !keys.contains($0.name) }
            route?.queryItems = retained + patch
            parts?.fragment = route?.string
        } else { return nil }
        return parts?.url
    }
    func reload() {
        error = nil; jumps.started()
        if let url = webView.url, isLocal(url) { webView.reload() }
        else if let origin { webView.load(URLRequest(url: origin)) }
    }
    func setVisible(_ visible: Bool) {
        self.visible = visible
        webView.evaluateJavaScript("window.__tongdaoSetVisible?.(\(visible ? "true" : "false"))", completionHandler: nil)
    }
    func isLocal(_ url: URL) -> Bool {
        guard let origin else { return false }
        return url.scheme == origin.scheme && url.host == origin.host && url.port == origin.port
    }
    func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
        error = nil; setVisible(visible)
        jumps.finished().forEach(send)
    }
    func webView(_ webView: WKWebView, didFail navigation: WKNavigation!, withError error: Error) { loadFailed(error) }
    func webView(_ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!, withError error: Error) { loadFailed(error) }
    private func loadFailed(_ failure: Error) {
        guard (failure as NSError).code != NSURLErrorCancelled else { return }
        error = "页面加载失败，可以重新载入。"
    }
    func webViewWebContentProcessDidTerminate(_ webView: WKWebView) {
        jumps.started()
        error = "界面进程已退出；后台连接未因此重启，请重新载入页面。"
    }
    func webView(_ webView: WKWebView, decidePolicyFor action: WKNavigationAction, decisionHandler: @escaping (WKNavigationActionPolicy) -> Void) {
        guard let url = action.request.url else { decisionHandler(.cancel); return }
        let localBlob = url.scheme == "blob" && URL(string: String(url.absoluteString.dropFirst(5))).map(isLocal) == true
        if action.targetFrame?.isMainFrame == false {
            // noVNC owns a separate loopback port; it never replaces the management origin.
            decisionHandler(["http", "https"].contains(url.scheme ?? "") && url.host == "127.0.0.1" || url.absoluteString == "about:blank" ? .allow : .cancel)
        } else if isLocal(url) || localBlob {
            decisionHandler(action.shouldPerformDownload ? .download : .allow)
        } else {
            if action.navigationType == .linkActivated, ["http", "https"].contains(url.scheme ?? "") { NSWorkspace.shared.open(url) }
            decisionHandler(.cancel)
        }
    }
    func webView(_ webView: WKWebView, decidePolicyFor response: WKNavigationResponse, decisionHandler: @escaping (WKNavigationResponsePolicy) -> Void) {
        decisionHandler(response.canShowMIMEType ? .allow : .download)
    }
    func webView(_ webView: WKWebView, createWebViewWith configuration: WKWebViewConfiguration, for action: WKNavigationAction, windowFeatures: WKWindowFeatures) -> WKWebView? {
        if let url = action.request.url, isLocal(url) { webView.load(action.request) }
        return nil
    }
    func webView(_ webView: WKWebView, runOpenPanelWith parameters: WKOpenPanelParameters, initiatedByFrame frame: WKFrameInfo, completionHandler: @escaping ([URL]?) -> Void) {
        guard frame.isMainFrame, let window = webView.window else { completionHandler(nil); return }
        let panel = NSOpenPanel(); panel.allowsMultipleSelection = parameters.allowsMultipleSelection
        panel.canChooseDirectories = parameters.allowsDirectories; panel.canChooseFiles = true
        panel.beginSheetModal(for: window) { response in completionHandler(response == .OK ? panel.urls : nil) }
    }
    func webView(_ webView: WKWebView, runJavaScriptAlertPanelWithMessage message: String, initiatedByFrame frame: WKFrameInfo, completionHandler: @escaping () -> Void) {
        guard frame.isMainFrame, let window = webView.window else { completionHandler(); return }
        let alert = NSAlert(); alert.messageText = message; alert.addButton(withTitle: "确定")
        alert.beginSheetModal(for: window) { _ in completionHandler() }
    }
    func webView(_ webView: WKWebView, runJavaScriptConfirmPanelWithMessage message: String, initiatedByFrame frame: WKFrameInfo, completionHandler: @escaping (Bool) -> Void) {
        guard frame.isMainFrame, let window = webView.window else { completionHandler(false); return }
        let alert = NSAlert(); alert.messageText = message; alert.addButton(withTitle: "确定"); alert.addButton(withTitle: "取消")
        alert.beginSheetModal(for: window) { completionHandler($0 == .alertFirstButtonReturn) }
    }
    func webView(_ webView: WKWebView, navigationAction: WKNavigationAction, didBecome download: WKDownload) { retain(download) }
    func webView(_ webView: WKWebView, navigationResponse: WKNavigationResponse, didBecome download: WKDownload) { retain(download) }
    private func retain(_ download: WKDownload) { downloads[ObjectIdentifier(download)] = download; download.delegate = self }
    func download(_ download: WKDownload, decideDestinationUsing response: URLResponse, suggestedFilename: String, completionHandler: @escaping (URL?) -> Void) {
        guard let window = webView.window else { completionHandler(nil); return }
        let panel = NSSavePanel(); panel.nameFieldStringValue = URL(fileURLWithPath: suggestedFilename).lastPathComponent
        panel.beginSheetModal(for: window) { result in completionHandler(result == .OK ? Self.replacing(panel.url) : nil) }
    }
    /// The save panel already asked before replacing; WKDownload fails on an existing file,
    /// so a file the user chose to replace is removed first. Folders are never removed.
    nonisolated static func replacing(_ destination: URL?) -> URL? {
        guard let destination else { return nil }
        var isDirectory: ObjCBool = false
        if FileManager.default.fileExists(atPath: destination.path, isDirectory: &isDirectory), !isDirectory.boolValue {
            try? FileManager.default.removeItem(at: destination)
        }
        return destination
    }
    func downloadDidFinish(_ download: WKDownload) { downloads.removeValue(forKey: ObjectIdentifier(download)) }
    func download(_ download: WKDownload, didFailWithError failure: Error, resumeData: Data?) {
        downloads.removeValue(forKey: ObjectIdentifier(download))
        if (failure as NSError).code != NSURLErrorCancelled { error = "文件未保存完成，请重新导出。" }
    }
}
