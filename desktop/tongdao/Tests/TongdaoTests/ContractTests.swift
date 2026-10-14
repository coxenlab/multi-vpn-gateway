import XCTest
import SwiftUI
@testable import Tongdao

final class ContractTests: XCTestCase {
    func testGatewayFeedbackRequiresFreshChecksAndRespectsRuntimeLifecycle() throws {
        let now = Date(timeIntervalSince1970: 1_000)
        func feedback(_ patch: [String: Any] = [:]) throws -> GatewayFeedback {
            var object: [String: Any] = ["runtime": ["phase": "ready"], "gateway_health": "healthy", "gateway_checked_at_ms": 1_000_000,
                "proxy_port_reachable": true, "mihomo_status": "running"]
            object.merge(patch) { _, new in new }
            let system = try JSONDecoder().decode(SystemStatus.self, from: JSONSerialization.data(withJSONObject: object))
            return GatewayFeedback.read(system, error: nil, now: now)
        }
        XCTAssertEqual(try feedback().title, "入口检测通过")
        for stamp: Any in [NSNull(), 939_999, 1_000_001] {
            let value = try feedback(["gateway_checked_at_ms": stamp])
            XCTAssertEqual(value.title, "入口状态待确认"); XCTAssertEqual(value.action, .refresh)
            XCTAssertNotEqual(value.tone, .good)
        }
        for patch: [String: Any] in [["proxy_port_reachable": false], ["gateway_health": "new_unknown_state"], ["mihomo_status": "down"]] {
            XCTAssertNotEqual(try feedback(patch).tone, .good)
        }
        for phase in ["dormant", "starting", "releasing", "closing", "failed"] {
            let value = try feedback(["runtime": ["phase": phase], "gateway_health": "forward_dead", "gave_up": true])
            XCTAssertNil(value.detail, "Old gateway failures must not leak into \(phase)")
            XCTAssertNotEqual(value.action, .repair)
        }
        let unavailable = GatewayFeedback.read(nil, error: "HTTP 503", now: now)
        XCTAssertEqual(unavailable.title, "运行状态待确认"); XCTAssertEqual(unavailable.action, .refresh)
    }

    func testGatewayFeedbackSeparatesRecoveryManagementAndRuleFailures() throws {
        let now = Date(timeIntervalSince1970: 1_000)
        func feedback(_ patch: [String: Any]) throws -> GatewayFeedback {
            var object: [String: Any] = ["runtime": ["phase": "ready"], "gateway_health": "healthy", "gateway_checked_at_ms": 1_000_000,
                "proxy_port_reachable": true, "mihomo_status": "running"]
            object.merge(patch) { _, new in new }
            return GatewayFeedback.read(try JSONDecoder().decode(SystemStatus.self, from: JSONSerialization.data(withJSONObject: object)), error: nil, now: now)
        }
        for health in ["forward_dead", "transport_dead"] {
            let healing = try feedback(["gateway_health": health, "healing": true, "gave_up": true])
            XCTAssertEqual(healing.title, "正在修复分流连接"); XCTAssertNotEqual(healing.action, .repair)
            XCTAssertEqual(try feedback(["gateway_health": health, "gave_up": true]).action, .repair)
            XCTAssertEqual(try feedback(["gateway_health": health, "self_heal_enabled": false]).action, .repair)
            XCTAssertNotEqual(try feedback(["gateway_health": health, "self_heal_enabled": true]).action, .repair)
            XCTAssertNotEqual(try feedback(["gateway_health": health]).action, .repair)
        }
        for health in ["transport_degraded", "vm_down"] {
            let value = try feedback(["gateway_health": health])
            XCTAssertNotEqual(value.action, .repair); XCTAssertNotEqual(value.tone, .good)
        }
        XCTAssertEqual(try feedback(["gateway_health": "container_down"]).action, .repair)
        XCTAssertNotEqual(try feedback(["healing": true]).tone, .good)
        XCTAssertEqual(try feedback(["vm_egress_dead": true]).title, "运行环境出站检测失败")
        XCTAssertEqual(try feedback(["egress_guard_applied": false]).title, "基础网络防护待确认")
        XCTAssertEqual(try feedback(["routing_off": true]).action, .settings)
        XCTAssertEqual(try feedback(["config_application": ["available": true, "pending": true]]).action, .rules)
    }

    func testRuntimeFeedbackKeepsFailureAndDoesNotTreatSlowPreparationAsFailure() throws {
        func runtime(_ json: String) throws -> Runtime { try JSONDecoder().decode(Runtime.self, from: Data(json.utf8)) }
        let slow = try runtime(#"{"phase":"starting","detail":"正在下载运行环境文件…","progress_age_seconds":90}"#)
        XCTAssertTrue(slow.working); XCTAssertTrue(slow.quietPreparation)
        XCTAssertFalse(slow.canConnect); XCTAssertFalse(slow.ready)
        XCTAssertEqual(slow.notice, "正在下载运行环境文件…")
        let failed = try runtime(#"{"phase":"failed","detail":"正在下载旧阶段","error":"磁盘空间不足","progress_age_seconds":200}"#)
        XCTAssertTrue(failed.canConnect); XCTAssertFalse(failed.working); XCTAssertFalse(failed.quietPreparation)
        XCTAssertEqual(failed.notice, "磁盘空间不足")
        for phase in ["ready", "waiting", "releasing", "closing", "unknown"] {
            let value = try runtime("{\"phase\":\"\(phase)\"}")
            XCTAssertFalse(value.canConnect); XCTAssertFalse(value.quietPreparation)
        }
        XCTAssertTrue(try runtime(#"{"phase":"dormant"}"#).canConnect)
        XCTAssertFalse(try runtime(#"{"phase":"starting","progress_age_seconds":89}"#).quietPreparation)
        XCTAssertNotNil(try runtime(#"{"phase":"failed","error":""}"#).notice)
    }

    func testApplicationStatusDecodesPendingState() throws {
        let system = try JSONDecoder().decode(SystemStatus.self, from: fixture("native-system.json"))
        let state = try XCTUnwrap(system.config_application)
        XCTAssertTrue(state.available); XCTAssertTrue(state.pending)
        let ready = try JSONDecoder().decode(SystemStatus.self, from: Data("{\"runtime\":{\"phase\":\"ready\"},\"mihomo_status\":\"running\",\"config_application\":{\"available\":true,\"pending\":false,\"verified_at\":1}}".utf8))
        XCTAssertEqual(ready.config_application?.pending, false); XCTAssertEqual(ready.config_application?.verified_at, 1)
    }

    func testOperationMessageKeepsPartialSuccessUnconfirmed() throws {
        XCTAssertThrowsError(try operationMessage(["ok":false,"error":"未能修复入口"], fallback: "入口已修复")) { error in
            XCTAssertEqual(error.localizedDescription, "未能修复入口")
        }
        XCTAssertEqual(try operationMessage(["ok":true,"config_application":["pending":true]], fallback:"规则已同步"), "设置已保存，规则同步尚未确认")
        XCTAssertEqual(try operationMessage(["deferred":true], fallback:"规则已同步"), "已保存，连接后应用")
        XCTAssertEqual(try operationMessage(["ok":true,"config_application":["pending":false]], fallback:"规则已同步"), "规则已同步")
    }

    private func fixture(_ name: String) throws -> Data {
        guard let directory = ProcessInfo.processInfo.environment["TONGDAO_NATIVE_FIXTURE_DIR"] else {
            throw XCTSkip("真实 core API 合同验证需要隔离夹具输出目录")
        }
        return try Data(contentsOf: URL(fileURLWithPath: directory).appendingPathComponent(name))
    }
    func testDecodesActualCoreChannelsAndAdapterContracts() throws {
        let channels = try JSONDecoder().decode([Channel].self, from: fixture("native-channels.json"))
        let adapters = try JSONDecoder().decode([Adapter].self, from: fixture("native-adapters.json"))
        let system = try JSONDecoder().decode(SystemStatus.self, from: fixture("native-system.json"))
        XCTAssertEqual(channels.count, 1)
        XCTAssertEqual(channels[0].rules[0].pattern, "native.example")
        XCTAssertEqual(channels[0].rules[0].enabled, 1)
        XCTAssertEqual(system.runtime?.phase, "dormant")
        XCTAssertTrue(adapters.contains { $0.key == "easyconnect" && $0.inputs.contains { $0.secret == true } })
        var channel = channels[0]
        channel.stop_pending = true; channel.status = "logged_in"
        XCTAssertTrue(channel.needsStart)
        XCTAssertEqual(channel.statusLabel(runtime: system.runtime), "停用待确认")
    }
}
