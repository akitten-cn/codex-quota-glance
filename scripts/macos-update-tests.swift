import Foundation

@main
struct MacUpdateTests {
    static func main() throws {
        func expectFailure(_ block: () throws -> Void) {
            do { try block(); fatalError("应拒绝无效更新数据") } catch { }
        }
        let newer = try MacRelease.isNewer("v0.2.6", than: "0.2.5")
        let older = try MacRelease.isNewer("v0.2.4", than: "0.2.5")
        precondition(newer && !older)
        expectFailure { _ = try MacRelease.isNewer("../../bad", than: "0.2.5") }
        let digest = String(repeating: "ab", count: 32)
        let line = "\(digest)  dist/codex-taskbar-0.2.6-macos-arm64.dmg\n"
        let release = try MacRelease.checksums(Data(line.utf8), arch: "arm64")
        let asset = try release.asset(arch: "arm64")
        precondition(asset.digest == "sha256:" + digest)
        precondition(asset.browser_download_url == "https://github.com/akitten-cn/codex-quota-glance/releases/download/v0.2.6/codex-taskbar-0.2.6-macos-arm64.dmg")
        expectFailure { _ = try release.asset(arch: "x86_64") }
        expectFailure { _ = try MacRelease.checksums(Data((line + line).utf8), arch: "arm64") }
        expectFailure { _ = try MacRelease.checksums(Data(line.replacingOccurrences(of: digest, with: "invalid").utf8), arch: "arm64") }
        expectFailure { _ = try MacRelease.checksums(Data(line.replacingOccurrences(of: "0.2.6", with: "../../bad").utf8), arch: "arm64") }
        if case .rateLimited(403) = MacRelease.failure(status: 403, remaining: "0", body: Data()) { } else { fatalError("未识别 API 限流") }
        if case .forbidden = MacRelease.failure(status: 403, remaining: nil, body: Data()) { } else { fatalError("未识别访问拒绝") }

        let root = FileManager.default.temporaryDirectory.appendingPathComponent("codex-update-tests-" + UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let logger = HostDiagnostics(root: root)
        logger.record("info", "macos_host_started")
        logger.setLevel("off")
        logger.record("info", "must_not_appear")
        logger.setLevel("warn")
        logger.record("info", "must_not_appear")
        logger.record("warn", "macos_update_failed", ["code":"rate_limited"])
        let files = try FileManager.default.contentsOfDirectory(at: root.appendingPathComponent("logs"), includingPropertiesForKeys: nil)
        let lines = try files.flatMap { try String(contentsOf: $0, encoding: .utf8).split(separator: "\n").map(String.init) }
        precondition(lines.count == 2)
        for line in lines { _ = try JSONSerialization.jsonObject(with: Data(line.utf8)) }
        precondition(!lines.joined().contains("must_not_appear"))
        print("Mac 更新清单、版本、架构、摘要、限流与宿主日志回归通过")
    }
}
