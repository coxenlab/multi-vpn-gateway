import Foundation
import Combine

/// Page-owned reads: only the newest request in the current core session may update UI.
@MainActor final class PageRequest: ObservableObject {
    @Published private(set) var loading = false
    @Published private(set) var error: String?
    private var generation = UUID()
    private(set) var isActive = true

    func cancel() { generation = UUID(); loading = false }
    func activate() { isActive = true }
    func suspend() { isActive = false; cancel() }

    @discardableResult func run<Value>(
        model: AppModel,
        operation: (LocalAPI) async throws -> Value,
        receive: (Value) -> Void
    ) async -> Bool {
        guard isActive, let client = model.api, model.isCurrent(client), !Task.isCancelled else { return false }
        let current = UUID(); generation = current; loading = true; error = nil
        defer { if generation == current { loading = false } }
        do {
            let value = try await operation(client)
            guard generation == current, model.isCurrent(client), !Task.isCancelled else { return false }
            receive(value)
            return true
        } catch {
            guard generation == current, model.isCurrent(client), !Task.isCancelled, !(error is CancellationError) else { return false }
            self.error = error.localizedDescription
            return false
        }
    }
}
