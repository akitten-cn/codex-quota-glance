import Foundation
import CoreGraphics

@main
struct WindowPlacementTests {
    static func main() {
        var checked = 0
        for work in [CGRect(x: 0, y: 24, width: 1440, height: 876), CGRect(x: -1280, y: -200, width: 1280, height: 700)] {
            for x in [work.minX, work.midX - 220, work.maxX - 440] {
                for y in [work.minY, work.minY + 20, work.midY, work.maxY - 44] {
                    let bar = CGRect(x: x, y: y, width: 440, height: 44)
                    for size in [CGSize(width: 600, height: 130), CGSize(width: 820, height: 550), CGSize(width: 880, height: 640)] {
                        guard let placed = anchoredPlacement(size: size, anchor: bar, work: work) else { fatalError("Missing placement") }
                        precondition(work.contains(placed.frame), "Window escaped visible screen")
                        precondition(!bar.intersects(placed.frame), "Window covers capsule")
                        precondition(placed.above ? placed.frame.minY >= bar.maxY + 10 : placed.frame.maxY <= bar.minY - 10, "Missing gap")
                        checked += 1
                    }
                }
            }
        }
        let work = CGRect(x: 0, y: 0, width: 1000, height: 700)
        let top = anchoredPlacement(size: CGSize(width: 600, height: 130), anchor: CGRect(x: 20, y: 656, width: 440, height: 44), work: work)!
        precondition(!top.above, "Top-edge popup must flip below capsule")
        print("Window placement: \(checked) cases passed; top-edge flip passed")
    }
}
