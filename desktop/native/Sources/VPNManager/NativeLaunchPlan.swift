import Foundation

/// Resolves the installed app without creating data, copying images, or launching tools.
struct NativeLaunchPlan {
    let executable: URL
    let environment: [String: String]
    let workingDirectory: URL

    private struct BundleMode: Decodable {
        let schema: Int
        let mode: String
        let vm_cache_key: String
    }

    static let executableResources = [
        "vpnmgr-core", "runtime/bin/colima", "runtime/bin/limactl", "runtime/bin/lima", "runtime/bin/docker",
        "runtime/helper/vpnmgr-helper", "runtime/helper/mihomo",
    ]
    static let readableResources = [
        "runtime/share/lima/lima-guestagent.Linux-aarch64.gz",
        "static/index.html", "static/native-login.html", "static/css/app.css",
        "static/channel.html", "static/new-channel.html", "static/routing-table.html", "static/monitor.html",
        "static/clash-config.html", "static/containers.html", "static/env-check.html",
        "static/js/pages/index.js", "static/js/pages/channel.js", "static/js/pages/new-channel.js",
        "static/js/pages/routing-table.js", "static/js/pages/monitor.js", "static/js/pages/clash-config.js",
        "static/js/pages/containers.js", "static/js/pages/env-check.js",
        "static/js/pages/native-login.js", "static/js/api.js", "static/js/app.js",
        "static/js/vncText.js", "static/js/vnc-lifecycle.js", "static/vendor/novnc/core/rfb.js",
        "images/mihomo.tar.gz", "images/oss-vpn.tar.gz",
    ]

    static func production(resources: URL?, home: URL, inheritedEnvironment: [String: String], otherInstanceRunning: Bool) throws -> Self {
        guard !otherInstanceRunning else { throw APIError(message: "另一个 VPN 管理网关正在运行，请使用已打开的应用；切换版本前需先退出旧版。") }
        guard let resources, resources.isFileURL, home.isFileURL else { throw APIError(message: "无法定位应用组件或用户目录，请使用完整版本。") }
        let root = resources.resolvingSymlinksInPath().standardizedFileURL
        let manager = FileManager.default
        func check(_ relativePath: String, executable: Bool = false, directory: Bool = false) throws {
            let path = root.appendingPathComponent(relativePath).resolvingSymlinksInPath().standardizedFileURL
            guard path.path.hasPrefix(root.path + "/"),
                  let value = try? path.resourceValues(forKeys: [.isRegularFileKey, .isDirectoryKey, .fileSizeKey]),
                  directory ? value.isDirectory == true : (value.isRegularFile == true && (value.fileSize ?? 0) > 0),
                  manager.isReadableFile(atPath: path.path), !executable || manager.isExecutableFile(atPath: path.path) else {
                throw APIError(message: "应用组件缺失或不可用：\(relativePath)。请使用完整版本后重试。")
            }
        }
        for path in executableResources { try check(path, executable: true) }
        for path in readableResources { try check(path) }
        try check("runtime/share/lima/templates", directory: true)
        try check("bundle-mode.json")
        guard let package = try? JSONDecoder().decode(BundleMode.self, from: Data(contentsOf: root.appendingPathComponent("bundle-mode.json"))),
              package.schema == 1, ["lite", "with-vm"].contains(package.mode), isCacheKey(package.vm_cache_key) else {
            throw APIError(message: "应用版本信息无效，请重新获取完整安装包。")
        }
        if package.mode == "with-vm" {
            try check("vm-image", directory: true)
            let images = try manager.contentsOfDirectory(atPath: root.appendingPathComponent("vm-image").path)
            guard images == [package.vm_cache_key] else {
                throw APIError(message: "内置运行环境镜像与当前版本不一致，请重新获取带镜像的安装包。")
            }
            try check("vm-image/" + package.vm_cache_key)
        } else if (try? root.appendingPathComponent("vm-image").resourceValues(forKeys: [.isSymbolicLinkKey])) != nil {
            throw APIError(message: "轻量版含有多余的运行环境镜像，请重新获取完整安装包。")
        }

        // Persisted infra.json supplies ports and secrets. Shell/development overrides must
        // never redirect the installed app to another profile, Docker context, or data store.
        var environment = inheritedEnvironment.filter { key, _ in
            !["VPNMGR_", "MIHOMO_", "COLIMA_", "LIMA_", "DOCKER_"].contains(where: key.hasPrefix)
                && !["DATA_DIR", "STATIC_DIR", "UI_PORT", "VPN_NET", "HELPER_RES_DIR"].contains(key)
        }
        environment["HOME"] = home.path
        environment["PATH"] = root.appendingPathComponent("runtime/bin").path + ":/usr/bin:/bin:/usr/sbin:/sbin"
        environment["DATA_DIR"] = home.appendingPathComponent("Library/Application Support/com.vpnmgr.desktop").path
        environment["STATIC_DIR"] = root.appendingPathComponent("static").path
        environment["HELPER_RES_DIR"] = root.appendingPathComponent("runtime/helper").path
        environment["VPNMGR_BUNDLED_IMAGES_DIR"] = root.appendingPathComponent("images").path
        if package.mode == "with-vm" {
            environment["VPNMGR_BUNDLED_VM_IMAGE_DIR"] = root.appendingPathComponent("vm-image").path
        }
        environment["VPNMGR_VM_PROFILE"] = "vpnmgr"
        environment["VPN_NET"] = "vpnmgr_vpnnet"
        environment["VPNMGR_MANAGED_VM"] = "1"
        environment["VPNMGR_NATIVE_CHILD"] = "1"
        return Self(executable: root.appendingPathComponent("vpnmgr-core"), environment: environment, workingDirectory: root)
    }

    static func isCacheKey(_ name: String) -> Bool {
        name.utf8.count == 64 && name.utf8.allSatisfy { (48...57).contains($0) || (97...102).contains($0) }
    }
}
