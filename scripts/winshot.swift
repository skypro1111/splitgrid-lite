// Screenshot the largest on-screen window of a process: winshot <owner-name> <out.png>
// Lists windows via CoreGraphics, then hands the id to `screencapture -l`.
import CoreGraphics
import Foundation

let args = CommandLine.arguments
guard args.count >= 3 else { print("usage: winshot <owner> <out.png> [title]"); exit(2) }
let owner = args[1], out = args[2]
let title: String? = args.count > 3 ? args[3] : nil
let list = CGWindowListCopyWindowInfo([.optionAll], kCGNullWindowID) as? [[String: Any]] ?? []
let mine = list.filter { ($0[kCGWindowOwnerName as String] as? String) == owner && ($0[kCGWindowLayer as String] as? Int) == 0 && (title == nil || ($0[kCGWindowName as String] as? String) == title) }
func area(_ w: [String: Any]) -> Double {
    let b = w[kCGWindowBounds as String] as? [String: Double] ?? [:]
    return (b["Width"] ?? 0) * (b["Height"] ?? 0)
}
guard let win = mine.max(by: { area($0) < area($1) }), let id = win[kCGWindowNumber as String] as? Int else {
    print("no window for \(owner); owners: \(Set(list.compactMap { $0[kCGWindowOwnerName as String] as? String }).sorted())"); exit(1)
}
let p = Process()
p.executableURL = URL(fileURLWithPath: "/usr/sbin/screencapture")
p.arguments = ["-x", "-o", "-l\(id)", out]
try p.run(); p.waitUntilExit()
print("captured window \(id) of \(owner) -> \(out)")
