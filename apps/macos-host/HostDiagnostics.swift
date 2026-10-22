import Foundation

// 原生宿主在 Rust 日志初始化前也可能失败。只写固定事件和错误码，不写系统错误正文或用户路径。
final class HostDiagnostics {
    private let root: URL
    private let lock = NSLock()
    private var threshold = 2
    init(root: URL) { self.root = root }

    func setLevel(_ level: String) {
        lock.lock(); defer { lock.unlock() }
        threshold = ["trace":0, "debug":1, "info":2, "warn":3, "error":4, "off":5][level] ?? 2
    }
    func record(_ level: String, _ event: String, _ fields: [String: Any] = [:]) {
        lock.lock(); defer { lock.unlock() }
        guard (["info":2, "warn":3, "error":4][level] ?? 2) >= threshold else { return }
        let timestamp = ISO8601DateFormatter().string(from: Date())
        let directory = root.appendingPathComponent("logs", isDirectory: true)
        let url = directory.appendingPathComponent("codex-taskbar.jsonl.host.\(timestamp.prefix(10))")
        var safe = fields; safe["event"] = event
        do {
            try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
            var data = try JSONSerialization.data(withJSONObject: ["timestamp":timestamp, "level":level.uppercased(), "fields":safe, "target":"macos_host"])
            data.append(10)
            if !FileManager.default.fileExists(atPath: url.path) { FileManager.default.createFile(atPath: url.path, contents: nil) }
            let file = try FileHandle(forWritingTo: url)
            defer { try? file.close() }
            try file.seekToEnd(); try file.write(contentsOf: data)
        } catch { fputs("Codex Taskbar: host_log_write_failed\n", stderr) }
    }
}
