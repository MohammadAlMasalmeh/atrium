// Watches a test browser and proves it never became visible or frontmost.
// Usage: window_audit <pid> [--kill] [--interval-ms 50] [--seconds N] [--regular-grace S]
// Polls until <pid> exits (or N seconds pass), then prints one JSON summary line. A violation is the
// process becoming the frontmost app, owning an on-screen window with nonzero
// alpha, or keeping the Regular activation policy (a Dock icon) for --regular-grace
// seconds in a row; bundled apps are briefly Regular while checking in. With --kill,
// the process is SIGKILLed at the first violation. Exit status 1 on violation.
import AppKit
import CoreGraphics
import Foundation

var args = Array(CommandLine.arguments.dropFirst())
guard let first = args.first, let pid = pid_t(first) else {
    FileHandle.standardError.write("usage: window_audit <pid> [--kill] [--interval-ms N]\n".data(using: .utf8)!)
    exit(2)
}
args.removeFirst()
let kill = args.contains("--kill")
var interval = 0.05
if let i = args.firstIndex(of: "--interval-ms"), i + 1 < args.count, let ms = Double(args[i + 1]) { interval = ms / 1000 }
var regularGrace = 0.0
if let i = args.firstIndex(of: "--regular-grace"), i + 1 < args.count, let s = Double(args[i + 1]) { regularGrace = s }
var limit = Double.infinity
if let i = args.firstIndex(of: "--seconds"), i + 1 < args.count, let s = Double(args[i + 1]) { limit = s }

func alive() -> Bool { Darwin.kill(pid, 0) == 0 }
func frontmost() -> pid_t {
    NSWorkspace.shared.frontmostApplication?.processIdentifier ?? -1
}
func visibleWindows() -> [[String: Any]] {
    guard let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]] else { return [] }
    return list.compactMap { w in
        guard (w[kCGWindowOwnerPID as String] as? Int).map(pid_t.init) == pid else { return nil }
        let alpha = w[kCGWindowAlpha as String] as? Double ?? 1
        guard alpha > 0 else { return nil }
        let b = w[kCGWindowBounds as String] as? [String: Double] ?? [:]
        return ["alpha": alpha, "layer": w[kCGWindowLayer as String] as? Int ?? 0,
                "x": b["X"] ?? 0, "y": b["Y"] ?? 0, "w": b["Width"] ?? 0, "h": b["Height"] ?? 0]
    }
}
func policy() -> String {
    switch NSRunningApplication(processIdentifier: pid)?.activationPolicy {
    case .some(.regular): return "regular"
    case .some(.accessory): return "accessory"
    case .some(.prohibited): return "prohibited"
    default: return "not-registered"
    }
}

let started = Date()
let frontBefore = frontmost()
var polls = 0
var violations: [[String: Any]] = []
var policies = Set<String>()
var timeline: [[String: Any]] = []
var regularSince: Double? = nil
var maxFrontChanges = 0
var lastFront = frontBefore
while alive() && Date().timeIntervalSince(started) < limit {
    // Let NSWorkspace deliver frontmost-application updates to this CLI process.
    RunLoop.current.run(until: Date().addingTimeInterval(interval))
    polls += 1
    let front = frontmost()
    if front != lastFront { maxFrontChanges += 1; lastFront = front }
    let p = policy()
    policies.insert(p)
    let elapsed = Date().timeIntervalSince(started)
    if timeline.last?["policy"] as? String != p { timeline.append(["policy": p, "t": elapsed]) }
    var found: [String: Any] = [:]
    if front == pid { found["frontmost"] = true }
    if p == "regular" {
        if regularSince == nil { regularSince = elapsed }
        if elapsed - regularSince! >= regularGrace { found["activation_policy"] = p }
    } else {
        regularSince = nil
    }
    let windows = visibleWindows()
    if !windows.isEmpty { found["visible_windows"] = windows }
    if !found.isEmpty {
        found["t"] = Date().timeIntervalSince(started)
        violations.append(found)
        if kill { Darwin.kill(pid, SIGKILL) }
    }
}
let frontAfter = frontmost()
let summary: [String: Any] = [
    "pid": Int(pid), "polls": polls, "interval_ms": Int(interval * 1000),
    "seconds": Date().timeIntervalSince(started),
    "frontmost_before_was_test": frontBefore == pid, "frontmost_after_was_test": frontAfter == pid,
    "frontmost_unchanged": frontBefore == frontAfter, "frontmost_changes_seen": maxFrontChanges,
    "activation_policies_seen": Array(policies).sorted(), "policy_timeline": timeline,
    "violations": violations, "ok": violations.isEmpty,
]
let data = try! JSONSerialization.data(withJSONObject: summary, options: [.sortedKeys])
print(String(data: data, encoding: .utf8)!)
exit(violations.isEmpty ? 0 : 1)
