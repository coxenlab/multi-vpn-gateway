// swift-tools-version: 5.9
import PackageDescription
let package = Package(name: "VPNManager", platforms: [.macOS(.v14)], products: [.executable(name: "VPNManager", targets: ["VPNManager"])], targets: [.executableTarget(name: "VPNManager"), .testTarget(name: "VPNManagerTests", dependencies: ["VPNManager"])])
