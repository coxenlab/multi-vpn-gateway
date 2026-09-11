import Foundation

/// Private disk staging keeps installer size independent of the application's memory usage.
struct InstallerUpload {
    static let maximumBytes = 1024 * 1024 * 1024
    let directory: URL
    let file: URL
    let boundary: String

    static func prepare(_ source: URL) async throws -> InstallerUpload {
        let values = try source.resourceValues(forKeys: [.fileSizeKey, .isRegularFileKey])
        guard values.isRegularFile == true, let size = values.fileSize, size > 0, size <= maximumBytes else {
            throw APIError(message: "请选择非空且不超过 1 GiB 的安装包")
        }
        let name = source.lastPathComponent
        guard !name.isEmpty, name != ".", name != "..", name.utf8.count <= 255,
              !name.contains(where: { $0 == "/" || $0 == "\\" || $0 == "\"" || $0.unicodeScalars.contains(where: { CharacterSet.controlCharacters.contains($0) }) }) else {
            throw APIError(message: "安装包文件名含不支持的字符，请重命名后上传")
        }
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("vpnmgr-upload-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false, attributes: [.posixPermissions: 0o700])
        let staged = InstallerUpload(directory: directory, file: directory.appendingPathComponent("body"), boundary: UUID().uuidString)
        do {
            guard FileManager.default.createFile(atPath: staged.file.path, contents: nil, attributes: [.posixPermissions: 0o600]) else { throw APIError(message: "无法暂存安装包，请检查可用磁盘空间") }
            let input = try FileHandle(forReadingFrom: source)
            defer { try? input.close() }
            let output = try FileHandle(forWritingTo: staged.file)
            defer { try? output.close() }
            try output.write(contentsOf: Data("--\(staged.boundary)\r\nContent-Disposition: form-data; name=\"file\"; filename=\"\(name)\"\r\nContent-Type: application/octet-stream\r\n\r\n".utf8))
            var copied = 0
            while true {
                try Task.checkCancellation()
                guard let chunk = try input.read(upToCount: 65536), !chunk.isEmpty else { break }
                copied += chunk.count
                guard copied <= maximumBytes else { throw APIError(message: "安装包超过 1 GiB") }
                try output.write(contentsOf: chunk)
                await Task.yield()
            }
            guard copied == size else { throw APIError(message: "安装包在读取时发生变化，请重新选择文件") }
            try output.write(contentsOf: Data("\r\n--\(staged.boundary)--\r\n".utf8))
            return staged
        } catch { staged.remove(); throw error }
    }

    func remove() { try? FileManager.default.removeItem(at: directory) }
}
