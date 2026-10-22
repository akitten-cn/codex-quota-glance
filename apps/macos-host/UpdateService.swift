import Foundation
import CFNetwork
import CryptoKit

// 所有网络操作在后台串行执行；每轮读取系统代理，兼容 Finder 启动时没有 Shell 代理变量。
// 仅下载公开 Release 资产，使用 GitHub digest 或官方清单校验；打开安装包而不替换正在运行的 .app。
enum UpdateFailure: Error, LocalizedError {
    case network(Int), http(Int), rateLimited(Int), forbidden, invalidManifest, missingAsset, missingDigest, invalidDigest, tooLarge, file
    var errorDescription: String? {
        switch self {
        case .network: return "更新连接失败，请检查系统代理和网络后重试。"
        case .http(let status): return "更新服务返回 HTTP \(status)。"
        case .rateLimited(let status): return "GitHub 请求频率受限（HTTP \(status)）；请稍后重试，或切换代理节点后再试。"
        case .forbidden: return "GitHub 拒绝访问（HTTP 403）；请检查系统代理规则是否包含 api.github.com 和 GitHub 下载域名。"
        case .invalidManifest: return "GitHub 更新清单无效。"
        case .missingAsset: return "最新正式版本没有适合此 Mac 架构的安装包；请等待对应版本发布。"
        case .missingDigest: return "安装包缺少 GitHub SHA-256 校验信息，已停止下载。"
        case .invalidDigest: return "安装包大小或 SHA-256 校验失败，请重试。"
        case .tooLarge: return "更新文件超过大小限制。"
        case .file: return "更新文件无法保存，请检查应用数据目录和磁盘空间。"
        }
    }
    var code: String {
        switch self {
        case .network: return "network"
        case .http: return "http"
        case .rateLimited: return "rate_limited"
        case .forbidden: return "forbidden"
        case .invalidManifest: return "manifest"
        case .missingAsset: return "asset_missing"
        case .missingDigest: return "digest_missing"
        case .invalidDigest: return "digest_mismatch"
        case .tooLarge: return "size_limit"
        case .file: return "file"
        }
    }
}

struct MacRelease: Decodable {
    struct Asset: Decodable {
        let name: String
        let browser_download_url: String
        let digest: String?
        let size: Int64
    }
    let tag_name: String
    let assets: [Asset]

    static func isNewer(_ candidate: String, than current: String) throws -> Bool {
        func parts(_ value: String) -> [Int]? {
            let raw = value.hasPrefix("v") || value.hasPrefix("V") ? String(value.dropFirst()) : value
            let segments = raw.split(separator: ".", omittingEmptySubsequences: false)
            guard segments.count == 3, segments.allSatisfy({ !$0.isEmpty && $0.utf8.allSatisfy({ $0 >= 48 && $0 <= 57 }) }) else { return nil }
            let values = segments.compactMap { Int($0) }
            return values.count == 3 && values.allSatisfy { $0 >= 0 } ? values : nil
        }
        guard let a = parts(candidate), let b = parts(current) else { throw UpdateFailure.invalidManifest }
        return b.lexicographicallyPrecedes(a)
    }

    func asset(arch: String) throws -> Asset {
        let candidates = assets.filter { asset in
            ["dmg", "zip"].contains { asset.name.hasSuffix("-macos-\(arch).\($0)") }
        }
        guard let asset = candidates.first(where: { $0.name.hasSuffix(".dmg") }) ?? candidates.first else { throw UpdateFailure.missingAsset }
        guard let url = URL(string: asset.browser_download_url), url.scheme == "https", url.host == "github.com",
              url.user == nil, url.password == nil, url.port == nil,
              url.path.hasPrefix("/akitten-cn/codex-quota-glance/releases/download/") else { throw UpdateFailure.invalidManifest }
        guard let digest = asset.digest, digest.hasPrefix("sha256:"), digest.count == 71,
              digest.dropFirst(7).allSatisfy({ $0.isHexDigit && $0.isASCII }) else { throw UpdateFailure.missingDigest }
        guard asset.size >= 0, asset.size <= 256 * 1024 * 1024 else { throw UpdateFailure.tooLarge }
        return asset
    }

    // 发布工作流的官方校验文件与 API 独立限流，固定到版本 URL 后仍验证完整摘要。
    static func checksums(_ data: Data, arch: String) throws -> MacRelease {
        guard let text = String(data: data, encoding: .utf8) else { throw UpdateFailure.invalidManifest }
        var assets: [Asset] = []
        var version: String?
        for line in text.split(whereSeparator: \.isNewline) {
            let fields = line.split(whereSeparator: \.isWhitespace)
            guard fields.count == 2 else { continue }
            var name = String(fields[1])
            if name.hasPrefix("*") { name.removeFirst() }
            if name.hasPrefix("dist/") { name = String(name.dropFirst(5)) }
            guard let ext = ["dmg", "zip"].first(where: { name.hasSuffix("-macos-\(arch).\($0)") }), name.hasPrefix("codex-taskbar-") else { continue }
            let candidate = String(name.dropFirst("codex-taskbar-".count).dropLast("-macos-\(arch).\(ext)".count))
            guard candidate.range(of: "^[0-9]+\\.[0-9]+\\.[0-9]+$", options: .regularExpression) != nil else { throw UpdateFailure.invalidManifest }
            _ = try isNewer(candidate, than: "0.0.0")
            guard version == nil || version == candidate, !assets.contains(where: { $0.name == name }) else { throw UpdateFailure.invalidManifest }
            version = candidate
            assets.append(Asset(name: name, browser_download_url: "https://github.com/akitten-cn/codex-quota-glance/releases/download/v\(candidate)/\(name)", digest: "sha256:" + String(fields[0]), size: 0))
        }
        guard let version = version, !assets.isEmpty else { throw UpdateFailure.missingAsset }
        let release = MacRelease(tag_name: "v" + version, assets: assets)
        _ = try release.asset(arch: arch)
        return release
    }

    static func failure(status: Int, remaining: String?, body: Data) -> UpdateFailure {
        let message = ((try? JSONSerialization.jsonObject(with: body)) as? [String: Any])?["message"] as? String ?? ""
        if status == 429 || (status == 403 && (remaining == "0" || message.lowercased().contains("rate limit") || message.lowercased().contains("abuse detection"))) {
            return .rateLimited(status)
        }
        return status == 403 ? .forbidden : .http(status)
    }
}

private final class UpdateTransfer: NSObject, URLSessionDownloadDelegate {
    let destination: URL
    let limit: Int64
    let done = DispatchSemaphore(value: 0)
    var failure: UpdateFailure?
    var response: HTTPURLResponse?
    var receivedFile = false

    init(destination: URL, limit: Int64) { self.destination = destination; self.limit = limit }

    static func allowed(_ url: URL) -> Bool {
        guard url.scheme == "https", let host = url.host?.lowercased(), url.user == nil, url.password == nil,
              url.port == nil || url.port == 443 else { return false }
        return host == "api.github.com" || host == "github.com" || host.hasSuffix(".githubusercontent.com")
    }

    func fetch(_ url: URL, log: (String, String, [String: Any]) -> Void) throws -> HTTPURLResponse {
        guard Self.allowed(url) else { throw UpdateFailure.invalidManifest }
        let config = URLSessionConfiguration.ephemeral
        let proxies = CFNetworkCopySystemProxySettings()?.takeRetainedValue() as? [AnyHashable: Any] ?? [:]
        config.connectionProxyDictionary = proxies
        config.timeoutIntervalForRequest = 30
        config.timeoutIntervalForResource = 900
        let mode = proxies[kCFNetworkProxiesProxyAutoConfigEnable as String] as? Int == 1 ? "system_pac" : "system"
        log("info", "macos_update_request", ["proxy_mode": mode])
        let session = URLSession(configuration: config, delegate: self, delegateQueue: nil)
        defer { session.invalidateAndCancel() }
        var request = URLRequest(url: url)
        request.setValue("Codex-Taskbar-Updater/1.0", forHTTPHeaderField: "User-Agent")
        if url.host == "api.github.com" {
            request.setValue("application/vnd.github+json", forHTTPHeaderField: "Accept")
            request.setValue("2022-11-28", forHTTPHeaderField: "X-GitHub-Api-Version")
        }
        session.downloadTask(with: request).resume()
        guard done.wait(timeout: .now() + 910) == .success else { throw UpdateFailure.network(NSURLErrorTimedOut) }
        if let failure = failure { throw failure }
        guard let response = response, receivedFile else { throw UpdateFailure.invalidManifest }
        log("info", "macos_update_response", ["status": response.statusCode, "proxy_mode": mode])
        if !(200...299).contains(response.statusCode) {
            let handle = try? FileHandle(forReadingFrom: destination)
            let body = (try? handle?.read(upToCount: 65536)) ?? Data()
            try? handle?.close()
            throw MacRelease.failure(status: response.statusCode, remaining: response.value(forHTTPHeaderField: "X-RateLimit-Remaining"), body: body)
        }
        return response
    }

    func urlSession(_ session: URLSession, task: URLSessionTask, willPerformHTTPRedirection response: HTTPURLResponse, newRequest request: URLRequest, completionHandler: @escaping (URLRequest?) -> Void) {
        guard let url = request.url, Self.allowed(url) else { failure = .invalidManifest; completionHandler(nil); return }
        completionHandler(request)
    }
    func urlSession(_ session: URLSession, downloadTask: URLSessionDownloadTask, didWriteData bytesWritten: Int64, totalBytesWritten: Int64, totalBytesExpectedToWrite: Int64) {
        if totalBytesWritten > limit || totalBytesExpectedToWrite > limit { failure = .tooLarge; downloadTask.cancel() }
    }
    func urlSession(_ session: URLSession, downloadTask: URLSessionDownloadTask, didFinishDownloadingTo location: URL) {
        guard failure == nil else { return }
        do {
            let size = (try FileManager.default.attributesOfItem(atPath: location.path)[.size] as? NSNumber)?.int64Value ?? 0
            guard size <= limit else { failure = .tooLarge; return }
            try FileManager.default.moveItem(at: location, to: destination)
            receivedFile = true
        } catch { failure = .file }
    }
    func urlSession(_ session: URLSession, task: URLSessionTask, didCompleteWithError error: Error?) {
        response = task.response as? HTTPURLResponse
        if let error = error, failure == nil { failure = .network((error as NSError).code) }
        done.signal()
    }
}

final class MacUpdateService {
    private var busy = false // 仅在主线程访问
    private let queue = DispatchQueue(label: "cn.akitten.codex-taskbar.update", qos: .utility)

    func run(download: Bool, root: URL, current: String, log: @escaping (String, String, [String: Any]) -> Void, progress: @escaping (String) -> Void, completion: @escaping (Result<URL?, UpdateFailure>) -> Void) {
        guard !busy else { progress("更新任务正在进行，请稍候。"); return }
        busy = true
        progress(download ? "正在检查并下载安装包…" : "正在检查 GitHub 最新版本…")
        queue.async {
            let result: Result<URL?, UpdateFailure>
            do { result = .success(try self.perform(download: download, root: root, current: current, log: log, progress: progress)) }
            catch let error as UpdateFailure { result = .failure(error) }
            catch { result = .failure(.file) }
            DispatchQueue.main.async { self.busy = false; completion(result) }
        }
    }

    private func perform(download: Bool, root: URL, current: String, log: (String, String, [String: Any]) -> Void, progress: @escaping (String) -> Void) throws -> URL? {
        let directory = root.appendingPathComponent("updates", isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let manifest = directory.appendingPathComponent(UUID().uuidString + ".json")
        defer { try? FileManager.default.removeItem(at: manifest) }
        #if arch(arm64)
        let arch = "arm64"
        #else
        let arch = "x86_64"
        #endif
        let release: MacRelease
        do {
            _ = try UpdateTransfer(destination: manifest, limit: 2 * 1024 * 1024).fetch(URL(string: "https://api.github.com/repos/akitten-cn/codex-quota-glance/releases/latest")!, log: log)
            guard let decoded = try? JSONDecoder().decode(MacRelease.self, from: Data(contentsOf: manifest)) else { throw UpdateFailure.invalidManifest }
            release = decoded
        } catch UpdateFailure.rateLimited(_), UpdateFailure.forbidden {
            log("info", "macos_update_checksum_fallback", [:])
            try? FileManager.default.removeItem(at: manifest)
            _ = try UpdateTransfer(destination: manifest, limit: 2 * 1024 * 1024).fetch(URL(string: "https://github.com/akitten-cn/codex-quota-glance/releases/latest/download/SHA256SUMS-macos-\(arch).txt")!, log: log)
            release = try MacRelease.checksums(Data(contentsOf: manifest), arch: arch)
        }
        guard try MacRelease.isNewer(release.tag_name, than: current) else {
            DispatchQueue.main.async { progress("已是最新正式版本（v\(current)）。") }
            return nil
        }
        let asset = try release.asset(arch: arch)
        guard download else {
            DispatchQueue.main.async { progress("发现新版本 \(release.tag_name)，可点击“下载并打开安装包”。") }
            return nil
        }
        DispatchQueue.main.async { progress("正在下载 \(release.tag_name)，下载后将校验并打开安装包。") }
        let destination = directory.appendingPathComponent(UUID().uuidString).appendingPathExtension(asset.name.hasSuffix(".dmg") ? "dmg" : "zip")
        var verified = false
        defer { if !verified { try? FileManager.default.removeItem(at: destination) } }
        _ = try UpdateTransfer(destination: destination, limit: asset.size > 0 ? asset.size : 256 * 1024 * 1024).fetch(URL(string: asset.browser_download_url)!, log: log)
        let size = (try FileManager.default.attributesOfItem(atPath: destination.path)[.size] as? NSNumber)?.int64Value
        guard asset.size == 0 || size == asset.size else { throw UpdateFailure.invalidDigest }
        let file = try FileHandle(forReadingFrom: destination)
        defer { try? file.close() }
        var hasher = SHA256()
        while let data = try file.read(upToCount: 1024 * 1024), !data.isEmpty { hasher.update(data: data) }
        let digest = hasher.finalize().map { String(format: "%02x", $0) }.joined()
        guard digest == asset.digest?.dropFirst(7).lowercased() else { throw UpdateFailure.invalidDigest }
        verified = true
        log("info", "macos_update_verified", ["bytes": size ?? 0])
        return destination
    }
}
