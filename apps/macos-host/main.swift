import AppKit
import WebKit

// 一个 .app、一个 UI 进程。Rust 辅助进程仅负责数据，不创建窗口或 Dock 图标。
func json(_ value: Any) -> String {
    guard let data = try? JSONSerialization.data(withJSONObject: value, options: [.fragmentsAllowed]),
          let text = String(data: data, encoding: .utf8) else { return "null" }
    return text.replacingOccurrences(of: "<", with: "\\u003c").replacingOccurrences(of: ">", with: "\\u003e")
}

final class FloatingPanel: NSPanel {
    var role = ""
    var didMove: (() -> Void)?
    private var down: NSEvent?
    private var dragged = false
    override var canBecomeKey: Bool { true }
    override var canBecomeMain: Bool { role == "settings" }
    override func sendEvent(_ event: NSEvent) {
        let point = event.locationInWindow
        if event.type == .leftMouseDown {
            down = event; dragged = false
            // 顶部真正的 26pt 拖拽带；右上关闭按钮及正文输入不被劫持。
            if role != "taskbar" && point.y > frame.height - 26 && point.x < frame.width - 65 {
                performDrag(with: event); didMove?(); return
            }
        }
        if role == "taskbar", event.type == .leftMouseDragged, let start = down,
           hypot(point.x - start.locationInWindow.x, point.y - start.locationInWindow.y) > 3 {
            dragged = true; performDrag(with: event); didMove?(); return
        }
        if event.type == .leftMouseUp { down = nil; if dragged { dragged = false; return } }
        super.sendEvent(event)
    }
}

final class Surface: NSObject, WKNavigationDelegate, WKUIDelegate {
    let panel: FloatingPanel
    let web: WKWebView
    let role: String
    var loaded = false
    var desiredVisible = true
    var onReady: (() -> Void)?
    init(role: String, size: NSSize, owner: AppDelegate, bootstrap: String) {
        self.role = role
        panel = FloatingPanel(contentRect: NSRect(origin: .zero, size: size), styleMask: [.borderless], backing: .buffered, defer: false)
        panel.role = role; panel.isReleasedWhenClosed = false; panel.isOpaque = false
        panel.backgroundColor = .clear; panel.hasShadow = false; panel.level = .floating
        panel.hidesOnDeactivate = false; panel.animationBehavior = .none
        panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]
        let config = WKWebViewConfiguration()
        let bridge = """
        (()=>{const listeners=[];window.chrome={webview:{addEventListener:(name,fn)=>{if(name==='message')listeners.push(fn)},postMessage:body=>window.webkit.messageHandlers.host.postMessage(body)}};window.__hostEmit=value=>listeners.forEach(fn=>fn({data:value}));})();
        \(bootstrap)
        """
        config.userContentController.addUserScript(WKUserScript(source: bridge, injectionTime: .atDocumentStart, forMainFrameOnly: true))
        config.userContentController.add(owner, name: "host")
        web = WKWebView(frame: NSRect(origin: .zero, size: size), configuration: config)
        super.init()
        web.navigationDelegate = self; web.uiDelegate = self
        web.setValue(false, forKey: "drawsBackground")
        web.autoresizingMask = [.width, .height]
        if #available(macOS 13.3, *) { web.isInspectable = ProcessInfo.processInfo.arguments.contains("--inspect") }
        let container = NSView(frame: web.frame)
        container.wantsLayer = true; container.layer?.masksToBounds = true
        container.layer?.cornerRadius = role == "taskbar" ? size.height / 2 : 26
        if role == "taskbar" {
            let glass = NSVisualEffectView(frame: web.frame)
            glass.material = .hudWindow; glass.blendingMode = .behindWindow; glass.state = .active
            glass.appearance = NSAppearance(named: .vibrantLight)
            glass.autoresizingMask = [.width, .height]; container.addSubview(glass)
        }
        container.addSubview(web); panel.contentView = container
    }
    func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
        loaded = true; onReady?()
    }
    func webView(_ webView: WKWebView, decidePolicyFor action: WKNavigationAction, decisionHandler: @escaping (WKNavigationActionPolicy) -> Void) {
        // UI 只加载捆绑页面。链接使用固定的宿主动作打开，不允许远程页面继承原生桥。
        let url = action.request.url
        decisionHandler(url == nil || url?.scheme == "about" || url?.isFileURL == true ? .allow : .cancel)
    }
    func webView(_ webView: WKWebView, runJavaScriptConfirmPanelWithMessage message: String, initiatedByFrame frame: WKFrameInfo, completionHandler: @escaping (Bool) -> Void) {
        let alert = NSAlert(); alert.messageText = message
        alert.addButton(withTitle: "确认"); alert.addButton(withTitle: "取消")
        alert.beginSheetModal(for: panel) { completionHandler($0 == .alertFirstButtonReturn) }
    }
    func emit(_ payload: Any) { guard loaded else { return }; web.evaluateJavaScript("window.__hostEmit(\(json(payload)))", completionHandler: nil) }
    func reveal(activate: Bool = false) {
        guard loaded else { return }
        if activate { NSApp.activate(ignoringOtherApps: true); panel.makeKeyAndOrderFront(nil) }
        else { panel.orderFrontRegardless() }
    }
    func hide() { desiredVisible = false; panel.orderOut(nil) }
}

final class AppDelegate: NSObject, NSApplicationDelegate, WKScriptMessageHandler {
    var surfaces: [String: Surface] = [:]
    var latest: [String: Any] = [:]
    var settings: [String: Any] = [:]
    var popup: Any?
    var engine: Process?
    let input = Pipe()
    let output = Pipe()
    var statusItem: NSStatusItem?
    var observers: [Any] = []
    var globalClick: Any?
    var localClick: Any?
    var popupTimer: Timer?
    var quitting = false
    var smoke = ProcessInfo.processInfo.arguments.contains("--smoke-test")
    var sawSettingsSaved = false
    var smokeFrames = 0
    var dataRoot: URL {
        if let path = ProcessInfo.processInfo.environment["CODEX_TASKBAR_DATA_DIR"] { return URL(fileURLWithPath: path) }
        return FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0].appendingPathComponent("CodexTaskbar")
    }
    func applicationDidFinishLaunching(_ notification: Notification) {
        if smoke { fputs("smoke: didFinishLaunching entered\n", stderr) }
        NSApp.setActivationPolicy(.accessory)
        statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        statusItem?.button?.title = "◉"
        statusItem?.button?.toolTip = "Codex Taskbar"
        statusItem?.menu = menu()
        observers.append(NotificationCenter.default.addObserver(forName: NSApplication.didChangeScreenParametersNotification, object: nil, queue: .main) { [weak self] _ in self?.screenChanged() })
        globalClick = NSEvent.addGlobalMonitorForEvents(matching: [.leftMouseDown, .rightMouseDown]) { [weak self] _ in self?.surfaces["details"]?.hide() }
        localClick = NSEvent.addLocalMonitorForEvents(matching: [.leftMouseDown, .rightMouseDown]) { [weak self] event in
            if event.window !== self?.surfaces["details"]?.panel && event.window !== self?.surfaces["taskbar"]?.panel { self?.surfaces["details"]?.hide() }
            return event
        }
        startEngine()
        if smoke {
            fputs("smoke: host launched\n", stderr)
            DispatchQueue.main.asyncAfter(deadline: .now() + 7) { self.runSmoke() }
            DispatchQueue.main.asyncAfter(deadline: .now() + 75) { if !self.quitting { self.alert("smoke: 超时") } }
        }
    }
    func startEngine() {
        if smoke { fputs("smoke: starting engine\n", stderr) }
        let process = Process()
        process.executableURL = Bundle.main.bundleURL.appendingPathComponent("Contents/MacOS/codex-taskbar-engine")
        process.arguments = ["--macos-bridge"]
        var environment = ProcessInfo.processInfo.environment
        environment["PATH"] = (environment["PATH"] ?? "/usr/bin:/bin:/usr/sbin:/sbin") + ":/opt/homebrew/bin:/usr/local/bin:" + FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".local/bin").path
        if smoke {
            environment["CODEX_TASKBAR_SMOKE_TEST"] = "1"
            environment["CODEX_HOME"] = dataRoot.appendingPathComponent("empty-codex-home").path
        }
        process.environment = environment; process.standardInput = input; process.standardOutput = output
        process.standardError = FileHandle.standardError
        process.terminationHandler = { [weak self] child in DispatchQueue.main.async {
            guard let self = self, !self.quitting else { return }
            self.alert("数据引擎已退出（\(child.terminationStatus)），请重新打开软件。")
        } }
        do { try process.run(); engine = process; if smoke { fputs("smoke: engine spawned\n", stderr) } } catch { alert("数据引擎启动失败：\(error.localizedDescription)"); return }
        DispatchQueue.global(qos: .utility).async { [weak self] in
            guard let self = self else { return }
            var pending = Data()
            while true {
                let bytes = self.output.fileHandleForReading.availableData
                if bytes.isEmpty { break }
                pending.append(bytes)
                if pending.count > 4 * 1024 * 1024 { pending.removeAll(); continue }
                while let end = pending.firstIndex(of: 10) {
                    let line = pending.subdata(in: pending.startIndex..<end)
                    pending.removeSubrange(pending.startIndex...end)
                    if let payload = (try? JSONSerialization.jsonObject(with: line)) as? [String: Any] {
                        DispatchQueue.main.async { self.receive(payload) }
                    }
                }
            }
        }
    }
    func send(_ command: [String: Any]) {
        guard engine?.isRunning == true, let bytes = (json(command) + "\n").data(using: .utf8) else { return }
        try? input.fileHandleForWriting.write(contentsOf: bytes)
    }
    func receive(_ payload: [String: Any]) {
        switch payload["kind"] as? String {
        case "state":
            latest = payload
            let next = payload["settings"] as? [String: Any] ?? [:]
            let changed = !NSDictionary(dictionary: next).isEqual(NSDictionary(dictionary: settings))
            let first = settings.isEmpty
            if first && smoke { fputs("smoke: first engine snapshot\n", stderr) }
            settings = next
            if first { createTaskbar() }
            if changed { placeTaskbar(restore: first); applyVisualSettings() }
            if let value = payload["taskbar"] { surfaces["taskbar"]?.emit(value) }
            if let value = payload["details"] { surfaces["details"]?.emit(value) }
        case "popup":
            guard let snapshot = payload["snapshot"], !(snapshot is NSNull) else { return }
            popup = snapshot
            // 消耗弹窗不能抢焦点或关闭用户正在看的详情/设置。
            guard surfaces["details"]?.panel.isVisible != true && surfaces["settings"]?.panel.isVisible != true else { return }
            showPopup()
        case "settings-result":
            sawSettingsSaved = payload["ok"] as? Bool == true
            surfaces["settings"]?.emit(payload)
        case "diagnostics-exported":
            NSWorkspace.shared.activateFileViewerSelecting([dataRoot.appendingPathComponent("diagnostics.json")])
        case "health": statusItem?.button?.toolTip = payload["message"] as? String
        default: break
        }
    }
    func resource(_ file: String) -> String {
        guard let url = Bundle.main.resourceURL?.appendingPathComponent(file), let text = try? String(contentsOf: url, encoding: .utf8) else { return "<html><body>页面资源缺失</body></html>" }
        return text
    }
    func fluid(_ cssClass: String) -> String {
        resource("fluid-front-reference.html")
            .replacingOccurrences(of: "<html", with: "<html class=\"\(cssClass)\"", options: [], range: nil)
            .replacingOccurrences(of: "<script src=\"taskbar-visual-contract.js\"></script>", with: "<script>\(resource("taskbar-visual-contract.js"))</script>")
    }
    func make(_ role: String, size: NSSize, html: String, bootstrap: String = "") -> Surface {
        let surface = Surface(role: role, size: size, owner: self, bootstrap: bootstrap)
        surfaces[role] = surface
        surface.onReady = { [weak self, weak surface] in
            guard let self = self, let surface = surface else { return }
            if self.smoke { fputs("smoke: loaded \(role)\n", stderr) }
            if role == "taskbar", let value = self.latest["taskbar"] { surface.emit(value); self.applyVisualSettings() }
            if role == "details", let value = self.latest["details"] { surface.emit(value) }
            if role == "popup", let value = self.popup { surface.emit(value) }
            self.applyVisualSettings()
            // 初始演示数据在窗口可见前被真实快照替换。
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.08) { if surface.desiredVisible { surface.reveal(activate: role == "settings") } }
        }
        let accessibleHTML = html.replacingOccurrences(of: "</head>", with: "<style>.mac-reduce-motion *,.mac-reduce-motion *::before,.mac-reduce-motion *::after{animation:none!important;transition:none!important}</style></head>")
        surface.web.loadHTMLString(accessibleHTML, baseURL: Bundle.main.resourceURL)
        return surface
    }
    func createTaskbar() {
        let width = CGFloat(settings["width"] as? Double ?? 440)
        let css = "<style>html{color-scheme:light}html.embed .bar{width:100%!important;height:100vh!important}html.embed .stage{padding:0!important}</style>"
        let surface = make("taskbar", size: NSSize(width: width, height: 44), html: fluid("embed").replacingOccurrences(of: "</head>", with: css + "</head>"), bootstrap: "window.__CodexTaskbarPhysicalWidth=\(width);")
        surface.panel.didMove = { [weak self] in
            guard let self = self else { return }
            self.clamp(surface.panel)
            UserDefaults.standard.set(NSStringFromRect(surface.panel.frame), forKey: "floatingFrame")
        }
    }
    func targetScreen() -> NSScreen? {
        if settings["display"] as? String == "secondary", NSScreen.screens.count > 1 { return NSScreen.screens[1] }
        return NSScreen.screens.first
    }
    func placeTaskbar(restore: Bool = false) {
        guard let surface = surfaces["taskbar"], let screen = targetScreen() else { return }
        let work = screen.visibleFrame, width = CGFloat(settings["width"] as? Double ?? 440)
        let offset = min(CGFloat(settings["traffic"] as? Double ?? 0), work.width / 2)
        let left = settings["dock"] as? String == "left"
        var rect = NSRect(x: left ? work.minX + 16 + offset : work.maxX - width - 16 - offset, y: work.minY + 20, width: width, height: 44)
        if restore, let saved = UserDefaults.standard.string(forKey: "floatingFrame") {
            let old = NSRectFromString(saved)
            if NSScreen.screens.contains(where: { $0.visibleFrame.intersects(old) }) { rect.origin = old.origin }
        }
        surface.panel.setFrame(rect, display: true); clamp(surface.panel)
    }
    func clamp(_ panel: NSWindow) {
        let screen = NSScreen.screens.first(where: { $0.visibleFrame.intersects(panel.frame) }) ?? targetScreen()
        guard let work = screen?.visibleFrame else { return }
        var rect = panel.frame
        rect.origin.x = max(work.minX, min(rect.minX, work.maxX - rect.width))
        rect.origin.y = max(work.minY, min(rect.minY, work.maxY - rect.height))
        panel.setFrame(rect, display: true)
    }
    func applyVisualSettings() {
        let opacity = settings["opacity"] as? Double ?? 70
        let frost = 0.13 + (100 - opacity) / 100 * 0.45
        surfaces["taskbar"]?.emit(["codexTaskbarPreviewWidth": settings["width"] ?? 440, "codexTaskbarPreviewFrost": frost])
        for surface in surfaces.values { surface.web.evaluateJavaScript("window.__macReduceMotion=\(settings["reduce_motion"] as? Bool == true ? "true" : "false");document.documentElement.classList.toggle('mac-reduce-motion',window.__macReduceMotion);", completionHandler: nil) }
    }
    func anchored(_ surface: Surface) {
        guard let bar = surfaces["taskbar"]?.panel else { return }
        var rect = surface.panel.frame
        rect.origin = NSPoint(x: bar.frame.midX - rect.width / 2, y: bar.frame.maxY + 10)
        surface.panel.setFrame(rect, display: true); clamp(surface.panel)
        if surface.role == "popup" {
            let arrow = max(24, min(rect.width - 24, bar.frame.midX - surface.panel.frame.minX))
            surface.web.evaluateJavaScript("document.querySelector('.consume-popover')?.style.setProperty('--arrow-x','\(arrow)px')", completionHandler: nil)
        }
    }
    @objc func showDetails() {
        surfaces["popup"]?.hide()
        if let existing = surfaces["details"] { existing.desiredVisible = true; anchored(existing); existing.reveal(); return }
        let work = (surfaces["taskbar"]?.panel.screen ?? targetScreen())?.visibleFrame ?? NSRect(x: 0, y: 0, width: 1440, height: 900)
        let size = NSSize(width: min(960, work.width - 30), height: min(720, work.height - 30))
        let html = resource("details-card-reference.html").replacingOccurrences(of: "<html", with: "<html class=\"details-embed\"")
        let surface = make("details", size: size, html: html)
        anchored(surface)
    }
    @objc func showSettings() {
        surfaces["popup"]?.hide(); surfaces["details"]?.hide()
        if let existing = surfaces["settings"] { existing.desiredVisible = true; existing.panel.center(); clamp(existing.panel); existing.reveal(activate: true); return }
        let work = (surfaces["taskbar"]?.panel.screen ?? targetScreen())?.visibleFrame ?? NSRect(x: 0, y: 0, width: 1440, height: 900)
        let size = NSSize(width: min(1040, work.width - 30), height: min(800, work.height - 30))
        var snapshot = settings
        snapshot["primary_work_width"] = NSScreen.screens.first?.visibleFrame.width ?? 1440
        snapshot["secondary_work_width"] = NSScreen.screens.count > 1 ? NSScreen.screens[1].visibleFrame.width : 0
        snapshot["has_secondary"] = NSScreen.screens.count > 1
        let bootstrap = "window.__CodexTaskbarSettingsEmbed=true;window.__CodexTaskbarSettingsSnapshot=\(json(snapshot));window.__CodexTaskbarPreviewDocument=\(json(fluid("embed")));"
        let css = "<style>html{color-scheme:light}html.settings-embed main{margin:0!important;width:100%!important;max-width:none!important;height:100%!important}.shell{box-shadow:none!important}.preview iframe{color-scheme:light}</style>"
        var html = resource("settings-layout-reference.html").replacingOccurrences(of: "<html", with: "<html class=\"settings-embed\"").replacingOccurrences(of: "</head>", with: css + "</head>")
        html = html.replacingOccurrences(of: "任务栏布局", with: "浮窗布局").replacingOccurrences(of: "任务栏宽度", with: "浮窗宽度").replacingOccurrences(of: "任务栏避让", with: "边缘偏移").replacingOccurrences(of: "默认优先放在副屏，并避让通知区域与 TrafficMonitor。", with: "桌面浮动显示，可直接拖动胶囊；无副屏时自动使用主屏。")
        html = html.replacingOccurrences(of: "自动使用 Windows 系统代理 / PAC", with: "更新页面使用系统浏览器与本机网络设置")
        // Mac 浮窗按逻辑点设置宽度；不能套用 Windows 的物理像素 / DPR 换算。
        html = html.replacingOccurrences(of: "const previewScale = Math.max(1, Number(window.devicePixelRatio) || 1);", with: "const previewScale = 1;")
            .replacingOccurrences(of: "58 / previewScale", with: "44 / previewScale")
            .replacingOccurrences(of: " px", with: " pt")
        let surface = make("settings", size: size, html: html, bootstrap: bootstrap)
        surface.panel.setFrameOrigin(NSPoint(x: work.midX - size.width / 2, y: work.midY - size.height / 2))
    }
    func showPopup() {
        popupTimer?.invalidate()
        let surface: Surface
        if let existing = surfaces["popup"] { surface = existing; surface.desiredVisible = true; if let popup = popup { surface.emit(popup) }; surface.reveal() }
        else { surface = make("popup", size: NSSize(width: min(600, (targetScreen()?.visibleFrame.width ?? 1000) - 30), height: 130), html: fluid("consume-embed")) }
        anchored(surface)
        popupTimer = Timer.scheduledTimer(withTimeInterval: 4, repeats: false) { [weak surface] _ in surface?.hide() }
    }
    func closeSettings() {
        guard let surface = surfaces.removeValue(forKey: "settings") else { return }
        surface.web.configuration.userContentController.removeScriptMessageHandler(forName: "host")
        surface.panel.close()
    }
    func screenChanged() {
        placeTaskbar()
        for surface in surfaces.values { clamp(surface.panel) }
        // 重建时注入最新的屏幕范围，不保留上一块屏幕的滑块上限。
        if surfaces["settings"]?.panel.isVisible == true { closeSettings(); showSettings() }
    }
    func userContentController(_ controller: WKUserContentController, didReceive message: WKScriptMessage) {
        guard message.frameInfo.isMainFrame, let body = message.body as? [String: Any], let action = body["action"] as? String else { return }
        switch action {
        case "show-details": if surfaces["details"]?.panel.isVisible == true { surfaces["details"]?.hide() } else { showDetails() }
        case "open-settings": showSettings()
        case "close-settings": closeSettings()
        case "show-menu": if let bar = surfaces["taskbar"] { menu().popUp(positioning: nil, at: NSEvent.mouseLocation, in: nil); bar.panel.orderFrontRegardless() }
        case "show-history": showDetails()
        case "check-updates", "download-update":
            NSWorkspace.shared.open(URL(string: "https://github.com/akitten-cn/codex-quota-glance/releases")!)
            surfaces["settings"]?.emit(["kind":"settings-result","ok":true,"message":"Mac 测试版暂不自动安装更新，已打开版本发布页。"])
        case "save-settings":
            var command = body
            if var values = body["settings"] as? [String: Any] {
                let screen = values["display"] as? String == "secondary" && NSScreen.screens.count > 1 ? NSScreen.screens[1] : NSScreen.screens.first
                values["traffic"] = max(0, min(values["traffic"] as? Double ?? 0, (screen?.visibleFrame.width ?? 1440) / 2))
                command["settings"] = values
            }
            send(command)
        case "refresh-details", "manual-refresh", "clear-history", "export-diagnostics": send(body)
        default: break
        }
    }
    func menu() -> NSMenu {
        let menu = NSMenu()
        for (title, action) in [("显示详情", #selector(showDetails)), ("设置…", #selector(showSettings)), ("重新定位浮窗", #selector(resetPosition)), ("退出", #selector(quit))] {
            let item = NSMenuItem(title: title, action: action, keyEquivalent: ""); item.target = self; menu.addItem(item)
        }
        return menu
    }
    @objc func resetPosition() { UserDefaults.standard.removeObject(forKey: "floatingFrame"); placeTaskbar(); surfaces["taskbar"]?.reveal() }
    @objc func quit() { NSApp.terminate(nil) }
    func alert(_ message: String) { if smoke { fputs(message + "\n", stderr); exit(1) }; let alert = NSAlert(); alert.messageText = message; alert.runModal() }
    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        if smoke { fputs("smoke: applicationShouldTerminate\n", stderr) }
        if quitting { return .terminateNow }; quitting = true; send(["action":"quit"])
        DispatchQueue.global().async {
            let deadline = Date().addingTimeInterval(3)
            while self.engine?.isRunning == true && Date() < deadline { Thread.sleep(forTimeInterval: 0.05) }
            if self.engine?.isRunning == true { self.engine?.terminate() }
            DispatchQueue.main.async { if self.smoke { fputs("smoke: replying terminate\n", stderr) }; sender.reply(toApplicationShouldTerminate: true) }
        }
        return .terminateLater
    }
    // 构建机上的真实 WKWebView 冒烟：不读取开发者登录/会话，不使用伪业务数据。
    func runSmoke() {
        fputs("smoke: opening details and settings\n", stderr)
        guard surfaces["taskbar"]?.loaded == true, !latest.isEmpty else { alert("smoke: 胶囊或数据桥未就绪"); return }
        showDetails(); showSettings()
        DispatchQueue.main.asyncAfter(deadline: .now() + 5) {
            guard self.surfaces["details"]?.loaded == true, let settingsSurface = self.surfaces["settings"], settingsSurface.loaded else { self.alert("smoke: 页面未加载"); return }
            settingsSurface.web.evaluateJavaScript("document.getElementById('widthRange').value='283';document.getElementById('widthRange').dispatchEvent(new Event('input'));document.getElementById('apply').click();") { _, error in
                if let error = error { self.alert("smoke: \(error)") }
            }
            DispatchQueue.main.asyncAfter(deadline: .now() + 3) {
                guard self.sawSettingsSaved, self.settings["width"] as? Int == 283 else { self.alert("smoke: 设置保存未回读"); return }
                self.closeSettings(); self.showSettings()
                DispatchQueue.main.asyncAfter(deadline: .now() + 3) { self.finishSmoke() }
            }
        }
    }
    func finishSmoke() {
        fputs("smoke: checking reopened settings\n", stderr)
        guard let surface = surfaces["settings"] else { alert("smoke: 设置重开失败"); return }
        surface.web.evaluateJavaScript("({width:document.getElementById('widthRange').value,buttons:document.querySelectorAll('button').length,scroll:document.querySelector('.content').scrollHeight,client:document.querySelector('.content').clientHeight})") { result, error in
            guard error == nil, let result = result as? [String: Any], result["width"] as? String == "283" else { self.alert("smoke: 设置重开后未持久化"); return }
            let report: [String: Any] = ["ok":true,"settings_persisted":true,"engine_running":self.engine?.isRunning == true,"webviews":self.surfaces.count,"settings_layout":result]
            do { try json(report).write(to: self.dataRoot.appendingPathComponent("macos-smoke-results.json"), atomically: true, encoding: .utf8) }
            catch { self.alert("smoke: 验收报告写入失败"); return }
            fputs("smoke: settings persisted, requesting quit\n", stderr)
            self.quit()
        }
    }
}

if ProcessInfo.processInfo.arguments.contains("--smoke-test") { fputs("smoke: entering AppKit\n", stderr) }
let app = NSApplication.shared
if !ProcessInfo.processInfo.arguments.contains("--smoke-test"), let identifier = Bundle.main.bundleIdentifier {
    let other = NSRunningApplication.runningApplications(withBundleIdentifier: identifier).first { $0.processIdentifier != ProcessInfo.processInfo.processIdentifier }
    if let other = other { other.activate(options: [.activateIgnoringOtherApps]); exit(0) }
}
let delegate = AppDelegate()
app.delegate = delegate
withExtendedLifetime(delegate) { app.run() }
