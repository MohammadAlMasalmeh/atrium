// Brings one app forward and reports its largest on-screen window as JSON:
// {"activated":bool,"frontmost":bool,"window":{"x":..,"y":..,"w":..,"h":..}|null}
// Used by bench_three_way.py, which needs each browser visible and in front.
import AppKit

let pid = pid_t(CommandLine.arguments[1])!
let app = NSRunningApplication(processIdentifier: pid)
let activated = app?.activate(options: [.activateAllWindows]) ?? false
Thread.sleep(forTimeInterval: 0.5)
let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly], kCGNullWindowID) as? [[String: Any]] ?? []
var best: [String: Int]? = nil
for w in list where (w[kCGWindowOwnerPID as String] as? Int32) == pid && (w[kCGWindowLayer as String] as? Int) == 0 {
    let b = w[kCGWindowBounds as String] as? [String: Double] ?? [:]
    let rect = ["x": Int(b["X"] ?? 0), "y": Int(b["Y"] ?? 0), "w": Int(b["Width"] ?? 0), "h": Int(b["Height"] ?? 0)]
    if rect["w"]! * rect["h"]! > (best.map { $0["w"]! * $0["h"]! } ?? 0) { best = rect }
}
let front = NSWorkspace.shared.frontmostApplication?.processIdentifier == pid
let out: [String: Any] = ["activated": activated, "frontmost": front, "window": best as Any? ?? NSNull()]
print(String(data: try! JSONSerialization.data(withJSONObject: out), encoding: .utf8)!)
