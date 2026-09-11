import XCTest
@testable import VPNManager

final class UploadTests: XCTestCase {
    func testInstallerStagingPreservesBinaryAndRemovesPrivateFiles() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("vpnmgr-upload-test-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: root) }
        let source = root.appendingPathComponent("客户端.run")
        let payload = Data(repeating: 255, count: 3 * 1024 * 1024)
        try payload.write(to: source)
        let staged = try await InstallerUpload.prepare(source)
        defer { staged.remove() }
        XCTAssertEqual((try FileManager.default.attributesOfItem(atPath: staged.directory.path)[.posixPermissions] as? NSNumber)?.intValue, 0o700)
        XCTAssertEqual((try FileManager.default.attributesOfItem(atPath: staged.file.path)[.posixPermissions] as? NSNumber)?.intValue, 0o600)
        let body = try Data(contentsOf: staged.file)
        let separator = try XCTUnwrap(body.range(of: Data("\r\n\r\n".utf8)))
        XCTAssertEqual(body[separator.upperBound..<(separator.upperBound + payload.count)], payload)
        let footer = Data("\r\n--\(staged.boundary)--\r\n".utf8)
        XCTAssertEqual(body.suffix(footer.count), footer)
        staged.remove()
        XCTAssertFalse(FileManager.default.fileExists(atPath: staged.directory.path))
        XCTAssertEqual(try Data(contentsOf: source), payload)

        for name in ["empty.run", "bad\"name.run", "large.run"] {
            let file = root.appendingPathComponent(name)
            FileManager.default.createFile(atPath: file.path, contents: name == "bad\"name.run" ? Data([1]) : Data())
            if name == "large.run" {
                let handle = try FileHandle(forWritingTo: file); try handle.truncate(atOffset: UInt64(InstallerUpload.maximumBytes + 1)); try handle.close()
            }
            do { let invalid = try await InstallerUpload.prepare(file); invalid.remove(); XCTFail("Invalid installer accepted") }
            catch { XCTAssertTrue(error is APIError) }
        }
        let cancelled = Task { try await InstallerUpload.prepare(source) }
        cancelled.cancel()
        do { let invalid = try await cancelled.value; invalid.remove(); XCTFail("Cancelled preparation succeeded") }
        catch { XCTAssertTrue(error is CancellationError) }
    }

    func testInstallerFileUploadOverRealHTTPAndFailureCleanup() async throws {
        guard let value = ProcessInfo.processInfo.environment["VPNMGR_NATIVE_UPLOAD_PORT"], let port = Int(value) else {
            throw XCTSkip("真实上传验证需要独立本机 HTTP 接收器")
        }
        let source = FileManager.default.temporaryDirectory.appendingPathComponent("vpnmgr-installer-test-\(UUID().uuidString).run")
        defer { try? FileManager.default.removeItem(at: source) }
        try Data(repeating: 255, count: 3 * 1024 * 1024).write(to: source)
        let api = LocalAPI(port: port)
        try await api.upload("/ok", file: source)
        for path in ["/failed", "/false-success"] {
            do { try await api.upload(path, file: source); XCTFail("Upload error must not look successful") }
            catch { XCTAssertTrue(error is APIError) }
        }
        await api.invalidate()
    }
}
