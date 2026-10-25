// Types real keystrokes into the frontmost app through the HID event tap, the path a
// physical keyboard takes. Usage: type_keys <pid> <count> <interval_ms> <click_x> <click_y>
// (a negative click_x types into whatever has focus, without clicking).
import AppKit

let a = CommandLine.arguments
let pid = pid_t(a[1])!, count = Int(a[2])!, interval = Double(a[3])! / 1000
let click = CGPoint(x: Double(a[4])!, y: Double(a[5])!)
_ = NSRunningApplication(processIdentifier: pid)?.activate(options: [.activateAllWindows])
Thread.sleep(forTimeInterval: 0.8)
guard NSWorkspace.shared.frontmostApplication?.processIdentifier == pid else {
    print("{\"error\":\"not frontmost\"}"); exit(1)
}
let source = CGEventSource(stateID: .hidSystemState)
for type in [CGEventType.leftMouseDown, .leftMouseUp] where click.x >= 0 {
    CGEvent(mouseEventSource: source, mouseType: type, mouseCursorPosition: click, mouseButton: .left)?.post(tap: .cghidEventTap)
    Thread.sleep(forTimeInterval: 0.05)
}
Thread.sleep(forTimeInterval: 0.5)
// a s d f j k l and space: home-row keys, no modifiers.
let keys: [CGKeyCode] = [0, 1, 2, 3, 38, 40, 37, 49]
for i in 0..<count {
    let key = keys[i % keys.count]
    CGEvent(keyboardEventSource: source, virtualKey: key, keyDown: true)?.post(tap: .cghidEventTap)
    Thread.sleep(forTimeInterval: 0.012)
    CGEvent(keyboardEventSource: source, virtualKey: key, keyDown: false)?.post(tap: .cghidEventTap)
    Thread.sleep(forTimeInterval: max(0, interval - 0.012))
}
print("{\"typed\":\(count)}")
