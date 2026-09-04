import AppKit

// 代码绘制的自有图标；构建时生成全部 Retina 尺寸，不依赖外部图片服务。
let output = URL(fileURLWithPath: CommandLine.arguments[1])
try FileManager.default.createDirectory(at: output, withIntermediateDirectories: true)
func color(_ r: CGFloat, _ g: CGFloat, _ b: CGFloat) -> NSColor {
    NSColor(srgbRed: r, green: g, blue: b, alpha: 1)
}
func png(_ width: Int, _ height: Int, _ url: URL, draw: () -> Void) throws {
    let bitmap = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: width, pixelsHigh: height,
        bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
        colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: bitmap)
    draw()
    NSGraphicsContext.restoreGraphicsState()
    try bitmap.representation(using: .png, properties: [:])!.write(to: url)
}
let iconset = output.appendingPathComponent("AppIcon.iconset")
try FileManager.default.createDirectory(at: iconset, withIntermediateDirectories: true)
for size in [16, 32, 128, 256, 512] {
    for scale in [1, 2] {
        let pixels = size * scale
        let suffix = scale == 2 ? "@2x" : ""
        try png(pixels, pixels, iconset.appendingPathComponent("icon_\(size)x\(size)\(suffix).png")) {
            let transform = NSAffineTransform()
            transform.scale(by: CGFloat(pixels) / 1024); transform.concat()
            let tile = NSBezierPath(roundedRect: NSRect(x: 72, y: 72, width: 880, height: 880), xRadius: 196, yRadius: 196)
            NSGradient(starting: color(0.08, 0.17, 0.26), ending: color(0.03, 0.08, 0.15))!.draw(in: tile, angle: -90)
            let capsule = NSBezierPath(roundedRect: NSRect(x: 150, y: 330, width: 724, height: 364), xRadius: 182, yRadius: 182)
            NSGraphicsContext.saveGraphicsState()
            capsule.addClip()
            color(0.23, 0.32, 0.43).setFill(); capsule.fill()
            let purple = NSBezierPath()
            purple.move(to: NSPoint(x: 120, y: 300)); purple.line(to: NSPoint(x: 650, y: 300))
            purple.curve(to: NSPoint(x: 650, y: 730), controlPoint1: NSPoint(x: 760, y: 435), controlPoint2: NSPoint(x: 535, y: 575))
            purple.line(to: NSPoint(x: 120, y: 730)); purple.close()
            NSGradient(starting: color(0.30, 0.13, 0.73), ending: color(0.67, 0.39, 1))!.draw(in: purple, angle: 90)
            let cyan = NSBezierPath()
            cyan.move(to: NSPoint(x: 120, y: 300)); cyan.line(to: NSPoint(x: 438, y: 300))
            cyan.curve(to: NSPoint(x: 438, y: 730), controlPoint1: NSPoint(x: 550, y: 435), controlPoint2: NSPoint(x: 322, y: 575))
            cyan.line(to: NSPoint(x: 120, y: 730)); cyan.close()
            NSGradient(starting: color(0, 0.53, 0.64), ending: color(0.30, 0.94, 0.95))!.draw(in: cyan, angle: 90)
            NSGraphicsContext.restoreGraphicsState()
            let graph = NSBezierPath()
            graph.move(to: NSPoint(x: 265, y: 484)); graph.line(to: NSPoint(x: 373, y: 484))
            graph.line(to: NSPoint(x: 440, y: 559)); graph.line(to: NSPoint(x: 504, y: 461))
            graph.line(to: NSPoint(x: 572, y: 533)); graph.line(to: NSPoint(x: 747, y: 533))
            graph.lineWidth = 27; graph.lineCapStyle = .round; graph.lineJoinStyle = .round
            NSColor.white.withAlphaComponent(0.94).setStroke(); graph.stroke()
        }
    }
}
try png(640, 400, output.appendingPathComponent("installer-background.png")) {
    NSGradient(starting: color(0.98, 0.99, 1), ending: color(0.84, 0.94, 0.97))!
        .draw(in: NSBezierPath(rect: NSRect(x: 0, y: 0, width: 640, height: 400)), angle: -25)
    func label(_ text: String, _ rect: NSRect, _ size: CGFloat, _ weight: NSFont.Weight) {
        let paragraph = NSMutableParagraphStyle(); paragraph.alignment = .center
        (text as NSString).draw(in: rect, withAttributes: [.font: NSFont.systemFont(ofSize: size, weight: weight),
            .foregroundColor: color(0.12, 0.24, 0.32), .paragraphStyle: paragraph])
    }
    label("Codex Taskbar", NSRect(x: 0, y: 315, width: 640, height: 42), 28, .semibold)
    label("拖动左侧应用到右侧「应用程序」", NSRect(x: 0, y: 280, width: 640, height: 30), 16, .regular)
    label("安装完成后，可推出此安装磁盘", NSRect(x: 0, y: 38, width: 640, height: 28), 13, .regular)
    let arrow = NSBezierPath(); arrow.lineWidth = 4; arrow.lineCapStyle = .round; arrow.lineJoinStyle = .round
    arrow.move(to: NSPoint(x: 290, y: 205)); arrow.line(to: NSPoint(x: 350, y: 205))
    arrow.move(to: NSPoint(x: 336, y: 219)); arrow.line(to: NSPoint(x: 350, y: 205)); arrow.line(to: NSPoint(x: 336, y: 191))
    color(0.16, 0.63, 0.71).setStroke(); arrow.stroke()
}
