import Foundation
import CoreGraphics

struct AnchoredPlacement {
    let frame: CGRect
    let above: Bool
}

// 先选择胶囊上下的可用空间，再决定窗口大小。不能先放置再 clamp，否则会推回胶囊上。
func anchoredPlacement(size: CGSize, anchor: CGRect, work: CGRect, gap: CGFloat = 10) -> AnchoredPlacement? {
    guard work.width > 0, work.height > 0, size.width > 0, size.height > 0 else { return nil }
    let aboveSpace = max(0, work.maxY - anchor.maxY - gap)
    let belowSpace = max(0, anchor.minY - work.minY - gap)
    let above = aboveSpace >= size.height || (belowSpace < size.height && aboveSpace >= belowSpace)
    let height = min(size.height, above ? aboveSpace : belowSpace)
    guard height > 0 else { return nil }
    let width = min(size.width, work.width)
    let x = max(work.minX, min(anchor.midX - width / 2, work.maxX - width))
    let y = above ? anchor.maxY + gap : anchor.minY - gap - height
    return AnchoredPlacement(frame: CGRect(x: x, y: y, width: width, height: height), above: above)
}
