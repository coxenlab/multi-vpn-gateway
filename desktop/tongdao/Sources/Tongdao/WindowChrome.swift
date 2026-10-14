import SwiftUI
import AppKit
import WebKit

/// Content runs under a transparent title bar; the page leaves the sidebar's top 50px
/// free for the traffic lights, so no separate title bar is drawn.
struct WindowObserver: NSViewRepresentable {
    let model: AppModel; let delegate: AppDelegate
    func makeNSView(context: Context) -> NSView { NSView() }
    func updateNSView(_ view: NSView, context: Context) {
        DispatchQueue.main.async {
            guard let window = view.window else { return }
            window.delegate = delegate
            window.titlebarAppearsTransparent = true
            window.titleVisibility = .hidden
            window.styleMask.insert(.fullSizeContentView)
            model.window = window
        }
    }
}

/// Native drag strip over the sidebar top. The page's `-webkit-app-region` is unverified
/// in WebKit, so at least this area always moves the window.
struct WindowDragArea: NSViewRepresentable {
    final class DragView: NSView {
        override var mouseDownCanMoveWindow: Bool { true }
        override func mouseDown(with event: NSEvent) { window?.performDrag(with: event) }
    }
    func makeNSView(context: Context) -> NSView { DragView() }
    func updateNSView(_ view: NSView, context: Context) {}
}

extension NSWindow {
    /// The page controller hosted in this window, if the workspace page is mounted.
    var workspaceController: WebWorkspaceController? {
        func find(_ view: NSView) -> WKWebView? {
            if let web = view as? WKWebView { return web }
            for child in view.subviews { if let web = find(child) { return web } }
            return nil
        }
        return contentView.flatMap(find)?.navigationDelegate as? WebWorkspaceController
    }
}
