import XCTest
@testable import VPNManager

final class NativeLaunchTests: XCTestCase {
    private func fixture(_ operation: (URL, URL) throws -> Void) throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("vpnmgr-launch-\(UUID().uuidString)")
        let resources = root.appendingPathComponent("完整 App.app/Contents/Resources")
        let home = root.appendingPathComponent("private-home")
        defer { try? FileManager.default.removeItem(at: root) }
        for path in NativeLaunchPlan.executableResources + NativeLaunchPlan.readableResources + ["vm-image/" + String(repeating: "a", count: 64)] {
            let file = resources.appendingPathComponent(path)
            try FileManager.default.createDirectory(at: file.deletingLastPathComponent(), withIntermediateDirectories: true)
            try Data("fixture".utf8).write(to: file)
            if NativeLaunchPlan.executableResources.contains(path) {
                try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: file.path)
            }
        }
        try FileManager.default.createDirectory(at: resources.appendingPathComponent("runtime/share/lima/templates"), withIntermediateDirectories: true)
        try operation(resources, home)
        XCTAssertFalse(FileManager.default.fileExists(atPath: home.path), "Resolving launch must not initialize data or VM caches")
    }

    func testProductionUsesOnlyBundleAndPersistedParameters() throws {
        try fixture { resources, home in
            let inherited = ["HOME":"/wrong-home", "PATH":"/developer/bin", "DATA_DIR":"/wrong-data", "STATIC_DIR":"/wrong-static",
                "MIHOMO_SECRET":"test-only", "MIHOMO_CTRL_URL":"http://wrong-host", "MIHOMO_CONFIG_PATH":"/wrong-config",
                "UI_PORT":"1234", "VPN_NET":"wrong_net", "VPNMGR_DEV_MODE":"1", "VPNMGR_VM_PROFILE":"default",
                "VPNMGR_CORE_PATH":"/wrong-core", "VPNMGR_BUNDLED_VM_IMAGE_DIR":"/wrong-cache", "HELPER_RES_DIR":"/wrong-helper",
                "COLIMA_HOME":"/wrong-colima", "LIMA_HOME":"/wrong-lima", "DOCKER_HOST":"unix:///wrong.sock", "DOCKER_CONTEXT":"default",
                "LANG":"zh_CN.UTF-8", "HTTPS_PROXY":"http://localhost:8080"]
            let plan = try NativeLaunchPlan.production(resources: resources, home: home, inheritedEnvironment: inherited, otherInstanceRunning: false)
            let base = resources.resolvingSymlinksInPath()
            XCTAssertEqual(plan.executable, base.appendingPathComponent("vpnmgr-core"))
            XCTAssertEqual(plan.workingDirectory, base)
            XCTAssertEqual(plan.environment["HOME"], home.path)
            XCTAssertEqual(plan.environment["DATA_DIR"], home.appendingPathComponent("Library/Application Support/com.vpnmgr.desktop").path)
            XCTAssertEqual(plan.environment["PATH"], base.appendingPathComponent("runtime/bin").path + ":/usr/bin:/bin:/usr/sbin:/sbin")
            XCTAssertEqual(plan.environment["STATIC_DIR"], base.appendingPathComponent("static").path)
            XCTAssertEqual(plan.environment["HELPER_RES_DIR"], base.appendingPathComponent("runtime/helper").path)
            XCTAssertEqual(plan.environment["VPNMGR_BUNDLED_IMAGES_DIR"], base.appendingPathComponent("images").path)
            XCTAssertEqual(plan.environment["VPNMGR_BUNDLED_VM_IMAGE_DIR"], base.appendingPathComponent("vm-image").path)
            XCTAssertEqual(plan.environment["VPNMGR_VM_PROFILE"], "vpnmgr")
            XCTAssertEqual(plan.environment["VPN_NET"], "vpnmgr_vpnnet")
            XCTAssertEqual(plan.environment["VPNMGR_MANAGED_VM"], "1")
            XCTAssertEqual(plan.environment["VPNMGR_NATIVE_CHILD"], "1")
            for name in ["MIHOMO_SECRET", "MIHOMO_CTRL_URL", "MIHOMO_CONFIG_PATH", "UI_PORT", "VPNMGR_DEV_MODE", "VPNMGR_CORE_PATH", "COLIMA_HOME", "LIMA_HOME", "DOCKER_HOST", "DOCKER_CONTEXT"] {
                XCTAssertNil(plan.environment[name], name)
            }
            XCTAssertEqual(plan.environment["LANG"], inherited["LANG"])
            XCTAssertEqual(plan.environment["HTTPS_PROXY"], inherited["HTTPS_PROXY"])
        }
    }

    func testMissingUnreadableAndEscapedComponentsFailBeforeDataCreation() throws {
        for path in ["vpnmgr-core", "runtime/bin/colima", "runtime/helper/mihomo", "static/native-login.html", "static/vendor/novnc/core/rfb.js", "images/oss-vpn.tar.gz"] {
            try fixture { resources, home in
                try FileManager.default.removeItem(at: resources.appendingPathComponent(path))
                XCTAssertThrowsError(try NativeLaunchPlan.production(resources: resources, home: home, inheritedEnvironment: [:], otherInstanceRunning: false)) {
                    XCTAssertTrue($0.localizedDescription.contains(path))
                }
            }
        }
        try fixture { resources, home in
            try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: resources.appendingPathComponent("runtime/bin/colima").path)
            XCTAssertThrowsError(try NativeLaunchPlan.production(resources: resources, home: home, inheritedEnvironment: [:], otherInstanceRunning: false))
        }
        try fixture { resources, home in
            let escaped = resources.deletingLastPathComponent().appendingPathComponent("external.html")
            try Data("external".utf8).write(to: escaped)
            let path = resources.appendingPathComponent("static/native-login.html")
            try FileManager.default.removeItem(at: path)
            try FileManager.default.createSymbolicLink(at: path, withDestinationURL: escaped)
            XCTAssertThrowsError(try NativeLaunchPlan.production(resources: resources, home: home, inheritedEnvironment: [:], otherInstanceRunning: false))
        }
        try fixture { resources, home in
            try Data().write(to: resources.appendingPathComponent("images/mihomo.tar.gz"))
            XCTAssertThrowsError(try NativeLaunchPlan.production(resources: resources, home: home, inheritedEnvironment: [:], otherInstanceRunning: false))
        }
    }

    func testOtherInstalledInstanceAndMissingVMCacheRefuseLaunch() throws {
        XCTAssertThrowsError(try NativeLaunchPlan.production(resources: nil, home: URL(fileURLWithPath: "/unused"), inheritedEnvironment: [:], otherInstanceRunning: true)) {
            XCTAssertTrue($0.localizedDescription.contains("另一个"))
        }
        try fixture { resources, home in
            try FileManager.default.removeItem(at: resources.appendingPathComponent("vm-image/" + String(repeating: "a", count: 64)))
            try Data("not-an-image".utf8).write(to: resources.appendingPathComponent("vm-image/README.txt"))
            XCTAssertThrowsError(try NativeLaunchPlan.production(resources: resources, home: home, inheritedEnvironment: [:], otherInstanceRunning: false)) {
                XCTAssertTrue($0.localizedDescription.contains("内置运行环境镜像"))
            }
        }
    }
}
