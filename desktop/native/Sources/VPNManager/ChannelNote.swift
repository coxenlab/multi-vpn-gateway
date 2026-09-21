import SwiftUI

@MainActor final class NoteDraft: ObservableObject {
    @Published var text = ""
    @Published private(set) var baseline = ""
    @Published private(set) var loaded = false
    @Published private(set) var busy = false
    @Published private(set) var latestConflict: String?
    @Published private(set) var error: String?
    var dirty: Bool { loaded && text != baseline }

    func load(discardDraft: Bool = false, fetch: () async throws -> String) async {
        guard !busy else { return }
        let initialDraft = text
        busy = true; defer { busy = false }
        do {
            let latest = try await fetch()
            if !loaded || !dirty || (discardDraft && text == initialDraft) { text = latest; baseline = latest; loaded = true; latestConflict = nil }
            else if latest != baseline { latestConflict = latest }
            error = nil
        } catch { self.error = "备忘读取失败，草稿已保留。请重试。" }
    }
    @discardableResult func save(write: (String, String) async throws -> Void, fetch: () async throws -> String) async -> Bool {
        guard loaded, dirty, !busy, latestConflict == nil else { return false }
        let submitted = text, expected = baseline
        busy = true; error = nil; defer { busy = false }
        do {
            try await write(submitted, expected)
            // Input made while awaiting the server remains an unsaved draft.
            baseline = submitted
            return true
        } catch let failure as APIError where failure.statusCode == 409 {
            latestConflict = (try? await fetch())
            error = "备忘已有新修改。草稿已保留，请核对最新内容。"
        } catch { self.error = "保存结果尚未确认，草稿已保留。请重新读取核对。" }
        return false
    }
    func keepDraftAfterReview() {
        guard let latestConflict else { return }
        baseline = latestConflict; self.latestConflict = nil; error = nil
    }
}

struct ChannelNoteView: View {
    @EnvironmentObject var model: AppModel
    let channelID: String
    @ObservedObject var draft: NoteDraft
    var body: some View {
        Section("备忘录") {
            TextEditor(text: $draft.text).frame(minHeight: 100).disabled(!draft.loaded)
            Text("加密保存在本机，只记录你手动写入的内容。").font(.caption).foregroundStyle(.secondary)
            if let error = draft.error { Text(error).font(.callout).foregroundStyle(.red) }
            if let latest = draft.latestConflict {
                DisclosureGroup("最新保存的备忘（你的草稿仍在上方）") { Text(latest.isEmpty ? "（空备忘）" : latest).textSelection(.enabled) }
                HStack {
                    Button("已核对，继续编辑草稿") { draft.keepDraftAfterReview() }
                    Button("放弃草稿并载入最新备忘") { Task { await load(discard: true) } }
                }.disabled(draft.busy)
            }
            HStack {
                Button(draft.busy ? "处理中…" : "保存备忘") { Task { await save() } }
                    .disabled(!draft.dirty || draft.busy || draft.latestConflict != nil || draft.text.unicodeScalars.count > 20000)
                Button("重新读取") { Task { await load() } }.disabled(draft.busy)
                if draft.dirty { Text("有未保存的修改").font(.caption).foregroundStyle(.secondary) }
            }
            if draft.text.unicodeScalars.count > 20000 { Text("备忘超过 20000 字符，请精简后保存。").foregroundStyle(.red) }
        }.task(id: channelID) { await load() }
    }
    private func fetch() async throws -> String {
        guard let api = model.api, model.isCurrent(api) else { throw APIError(message: "本地服务尚未就绪") }
        struct Note: Decodable { let note: String }
        let note = try await api.get("/api/channels/\(channelID)/note", as: Note.self).note
        guard model.isCurrent(api) else { throw CancellationError() }
        return note
    }
    private func load(discard: Bool = false) async { await draft.load(discardDraft: discard, fetch: fetch) }
    private func save() async {
        guard let api = model.api, model.isCurrent(api) else { return }
        if await draft.save(write: { note, expected in
            guard model.isCurrent(api) else { throw CancellationError() }
            _ = try await api.write("/api/channels/\(channelID)/note", method: "PUT", body: ["note": note, "expected_note": expected])
            guard model.isCurrent(api) else { throw CancellationError() }
        }, fetch: fetch), model.isCurrent(api) { model.message = draft.dirty ? "备忘已保存，后续输入仍在草稿中" : "备忘已保存" }
    }
}
