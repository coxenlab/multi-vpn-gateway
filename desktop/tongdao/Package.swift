// swift-tools-version: 5.9
import PackageDescription
let package = Package(name: "Tongdao", platforms: [.macOS(.v14)], products: [.executable(name: "Tongdao", targets: ["Tongdao"])], targets: [.executableTarget(name: "Tongdao"), .testTarget(name: "TongdaoTests", dependencies: ["Tongdao"])])
