import XCTest
@testable import Tongdao

final class AppSettingsTests: XCTestCase {
    private func directory(_ contents: String?) throws -> URL {
        let url = FileManager.default.temporaryDirectory.appendingPathComponent("tongdao-settings-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        addTeardownBlock { try? FileManager.default.removeItem(at: url) }
        if let contents { try Data(contents.utf8).write(to: url.appendingPathComponent("app-settings.json")) }
        return url
    }

    func testMissingFileKeepsRunning() throws {
        XCTAssertTrue(AppModel.keepRunningOnClose(dataDirectory: try directory(nil)))
        XCTAssertTrue(AppModel.keepRunningOnClose(dataDirectory: nil))
    }
    func testBrokenFileKeepsRunning() throws {
        for contents in ["{", "", "[]", #"{"keep_running_on_close":0}"#, #"{"keep_running_on_close":"false"}"#] {
            XCTAssertTrue(AppModel.keepRunningOnClose(dataDirectory: try directory(contents)), contents)
        }
    }
    func testFalseQuitsAndTrueOrAbsentKeepsRunning() throws {
        XCTAssertFalse(AppModel.keepRunningOnClose(dataDirectory: try directory(#"{"launch_at_login":false,"auto_connect":false,"keep_running_on_close":false,"resume_channels":[]}"#)))
        XCTAssertTrue(AppModel.keepRunningOnClose(dataDirectory: try directory(#"{"keep_running_on_close":true}"#)))
        XCTAssertTrue(AppModel.keepRunningOnClose(dataDirectory: try directory(#"{"auto_connect":true}"#)))
    }
}
