import Foundation
import Network

struct RuleConflict: Identifiable {
    let a: RuleItem; let b: RuleItem; let reason: String
    var id: String { a.id + "/" + b.id }
}
struct RuleAnalysisInput: Hashable { let channels: [Channel]; let off: Bool }
struct IPRange {
    let lower: [UInt8]; let upper: [UInt8]
    init?(_ value: String) {
        let pieces = value.split(separator: "/", omittingEmptySubsequences: false)
        guard (1...2).contains(pieces.count) else { return nil }
        let address = String(pieces[0])
        let bytes: [UInt8]
        if let ip = IPv4Address(address) { bytes = Array(ip.rawValue) }
        else if let ip = IPv6Address(address) { bytes = Array(ip.rawValue) }
        else { return nil }
        guard let prefix = pieces.count == 2 ? Int(pieces[1]) : bytes.count * 8, (0...(bytes.count * 8)).contains(prefix) else { return nil }
        var lo = bytes, hi = bytes
        for i in bytes.indices {
            let bits = min(8, max(0, prefix - i * 8))
            let mask: UInt8 = bits == 0 ? 0 : UInt8(truncatingIfNeeded: 0xff << (8 - bits))
            lo[i] &= mask; hi[i] |= ~mask
        }
        lower = lo; upper = hi
    }
    func overlaps(_ other: IPRange) -> Bool {
        lower.count == other.lower.count && !upper.lexicographicallyPrecedes(other.lower) && !other.upper.lexicographicallyPrecedes(lower)
    }
}
private final class DomainNode {
    var children: [String: DomainNode] = [:]
    var entries: [RuleItem] = []
}
func findRuleConflicts(_ input: RuleAnalysisInput) -> [RuleConflict] {
    guard !input.off else { return [] }
    let rows = input.channels.filter { ch in
        ch.routing_enabled && ch.stop_pending != true && !["stopped", "error", "down"].contains(ch.configured_status ?? ch.status)
    }.flatMap { ch in ch.rules.filter { $0.enabled != 0 }.map { RuleItem(channel: ch, rule: $0) } }
    var results: [RuleConflict] = []
    let root = DomainNode()
    let domains = rows.filter { $0.rule.kind != "ip" }.map { ($0, $0.rule.pattern.lowercased().split(separator: ".").reversed().map(String.init)) }.sorted { $0.1.count < $1.1.count }
    for (item, labels) in domains {
        var node = root
        for label in labels {
            for ancestor in node.entries where ancestor.channel.id != item.channel.id {
                results.append(RuleConflict(a: ancestor, b: item, reason: "域名后缀相互覆盖")); if results.count == 100 { return results }
            }
            if node.children[label] == nil { node.children[label] = DomainNode() }
            node = node.children[label]!
        }
        for previous in node.entries where previous.channel.id != item.channel.id {
            results.append(RuleConflict(a: previous, b: item, reason: "域名后缀相互覆盖")); if results.count == 100 { return results }
        }
        node.entries.append(item)
    }
    let networks = rows.filter { $0.rule.kind == "ip" }.compactMap { row in IPRange(row.rule.pattern).map { (row, $0) } }
        .sorted { a, b in a.1.lower.count == b.1.lower.count ? a.1.lower.lexicographicallyPrecedes(b.1.lower) : a.1.lower.count < b.1.lower.count }
    var active: [(RuleItem, IPRange)] = []
    for (item, range) in networks {
        active.removeAll { $0.1.lower.count != range.lower.count || $0.1.upper.lexicographicallyPrecedes(range.lower) }
        for (previous, previousRange) in active where previous.channel.id != item.channel.id && previousRange.overlaps(range) {
            results.append(RuleConflict(a: previous, b: item, reason: "IP 网段交叠")); if results.count == 100 { return results }
        }
        active.append((item, range))
    }
    return results
}
