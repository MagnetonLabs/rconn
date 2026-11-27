import SwiftUI

/// Simulator tests only (`RCONN_TEST_SERVER` in the launch environment): what the app sent
/// the computer and how its session went, shown in an invisible label the UI test reads.
/// Off (records nothing) otherwise.
final class TestProbe: ObservableObject {
    static let shared = TestProbe()
    let enabled = ProcessInfo.processInfo.environment["RCONN_TEST_SERVER"] != nil
    /// Buttons and scrolls sent, in order ("left-down", "left-up", "scroll"…).
    @Published private(set) var input: [String] = []
    /// Sessions that showed a picture (a reconnect adds one).
    @Published private(set) var pictures = 0
    /// Where the picture is on screen (points).
    @Published var pictureFrame = CGRect.zero

    var summary: String {
        let f = pictureFrame.integral
        return "pictures=\(pictures) picture=\(Int(f.minX)),\(Int(f.minY)),\(Int(f.width)),\(Int(f.height))"
            + " input=\(input.joined(separator: ","))"
    }

    /// `RCONN_RESET_INPUT` in the launch environment: start from the default input settings.
    static func resetInputSettingsIfAsked() {
        guard ProcessInfo.processInfo.environment["RCONN_RESET_INPUT"] != nil else { return }
        let d = UserDefaults.standard
        for key in d.dictionaryRepresentation().keys where key.hasPrefix("input.") {
            d.removeObject(forKey: key)
        }
    }

    func button(_ b: PointerButton, pressed: Bool) {
        let name: String
        switch b {
        case .left: name = "left"
        case .right: name = "right"
        case .middle: name = "middle"
        }
        record("\(name)-\(pressed ? "down" : "up")")
    }

    func scroll() {
        if input.last != "scroll" { record("scroll") }
    }

    func pictureShown() {
        guard enabled else { return }
        onMain { self.pictures += 1 }
    }

    private func record(_ s: String) {
        guard enabled else { return }
        onMain { self.input.append(s) }
    }

    private func onMain(_ f: @escaping () -> Void) {
        if Thread.isMainThread { f() } else { DispatchQueue.main.async(execute: f) }
    }
}

/// The probe's invisible label (nothing at all outside tests).
struct TestProbeLabel: View {
    @ObservedObject private var probe = TestProbe.shared

    var body: some View {
        if probe.enabled {
            Text(probe.summary)
                .font(.system(size: 1))
                .foregroundStyle(.clear)
                .frame(width: 1, height: 1)
                .accessibilityIdentifier("testProbe")
                .accessibilityLabel(probe.summary)
                .allowsHitTesting(false)
        }
    }
}
