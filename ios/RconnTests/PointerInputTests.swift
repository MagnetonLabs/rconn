import XCTest
@testable import Rconn

/// Records what the computer would receive.
private final class Recorder: InputSink {
    var events: [String] = []
    var lastPointer: (Float, Float)?

    func sendPointer(x: Float, y: Float) {
        lastPointer = (x, y)
        if events.last != "move" { events.append("move") }
    }

    func sendButton(button: PointerButton, pressed: Bool) {
        let name: String
        switch button {
        case .left: name = "left"
        case .right: name = "right"
        case .middle: name = "middle"
        }
        events.append("\(name)-\(pressed ? "down" : "up")")
    }

    func sendScroll(dx: Float, dy: Float) {
        events.append("scroll \(dy)")
    }

    var buttons: [String] { events.filter { $0.hasSuffix("-down") || $0.hasSuffix("-up") } }
}

final class PointerInputTests: XCTestCase {
    private var rec: Recorder!
    private var input: PointerInput!
    private var now: CFTimeInterval = 100

    override func setUp() {
        rec = Recorder()
        input = PointerInput()
        let r = rec!
        input.sink = { r }
        input.clock = { [unowned self] in self.now }
    }

    private func set(_ f: (inout InputSettings) -> Void) {
        var s = input.settings
        f(&s)
        input.settings = s
    }

    // The bug report: a tap on Start opened Windows' right-click menu.
    func testOneFingerTapIsExactlyALeftClickInEveryMode() {
        for mode in PointerMode.allCases {
            for longPress in [false, true] {
                for twoFinger in [false, true] {
                    rec.events = []
                    set {
                        $0.mode = mode
                        $0.longPressRightClick = longPress
                        $0.twoFingerTapRightClick = twoFinger
                    }
                    input.tap(at: CGPoint(x: 0.1, y: 0.9))
                    XCTAssertEqual(rec.buttons, ["left-down", "left-up"], "\(mode) \(longPress) \(twoFinger)")
                }
            }
        }
    }

    func testTouchModeTapClicksWhereTapped() {
        input.tap(at: CGPoint(x: 0.05, y: 0.95))
        XCTAssertEqual(rec.events, ["move", "left-down", "left-up"])
        XCTAssertEqual(rec.lastPointer?.0 ?? 0, 0.05, accuracy: 0.0001)
        XCTAssertEqual(rec.lastPointer?.1 ?? 0, 0.95, accuracy: 0.0001)
    }

    func testTrackpadModeTapClicksAtThePointer() {
        set { $0.mode = .trackpad }
        input.tap(at: CGPoint(x: 0.05, y: 0.95))
        XCTAssertEqual(rec.events, ["left-down", "left-up"])
        XCTAssertEqual(input.pointer, CGPoint(x: 0.5, y: 0.5))
    }

    func testTwoFingerTapRightClicksOnlyWhenOn() {
        input.twoFingerTap(at: CGPoint(x: 0.3, y: 0.3))
        XCTAssertEqual(rec.buttons, ["right-down", "right-up"])
        rec.events = []
        set { $0.twoFingerTapRightClick = false }
        input.twoFingerTap(at: CGPoint(x: 0.3, y: 0.3))
        XCTAssertEqual(rec.events, [])
    }

    func testHoldClicksLeftUnlessLongPressRightClicks() {
        input.holdBegan(at: CGPoint(x: 0.2, y: 0.2))
        input.holdEnded()
        XCTAssertEqual(rec.buttons, ["left-down", "left-up"])
        rec.events = []
        set { $0.longPressRightClick = true }
        input.holdBegan(at: CGPoint(x: 0.2, y: 0.2))
        input.holdEnded()
        XCTAssertEqual(rec.buttons, ["right-down", "right-up"])
    }

    func testHoldThenMoveDragsWithTheLeftButton() {
        set { $0.longPressRightClick = true }
        input.holdBegan(at: CGPoint(x: 0.2, y: 0.2))
        input.holdMoved(to: CGPoint(x: 0.4, y: 0.4), by: CGPoint(x: 0.2, y: 0.2))
        input.holdMoved(to: CGPoint(x: 0.5, y: 0.5), by: CGPoint(x: 0.1, y: 0.1))
        input.holdEnded()
        XCTAssertEqual(rec.buttons, ["left-down", "left-up"])
        XCTAssertEqual(input.pointer, CGPoint(x: 0.5, y: 0.5))
    }

    func testTouchModeOneFingerDragsAfterTheDelay() {
        input.panBegan(at: CGPoint(x: 0.1, y: 0.1))
        input.panMoved(to: CGPoint(x: 0.12, y: 0.1), by: .zero)
        XCTAssertEqual(rec.buttons, [], "not before the delay: it may become a pinch")
        now += PointerInput.dragDelay
        input.panMoved(to: CGPoint(x: 0.3, y: 0.1), by: .zero)
        input.panEnded()
        XCTAssertEqual(rec.buttons, ["left-down", "left-up"])
        XCTAssertEqual(input.pointer.x, 0.3, accuracy: 0.0001)
    }

    func testTouchModeQuickFlickStillDrags() {
        input.panBegan(at: CGPoint(x: 0.1, y: 0.1))
        input.panMoved(to: CGPoint(x: 0.2, y: 0.1), by: .zero)
        input.panEnded()
        XCTAssertEqual(rec.events, ["move", "left-down", "move", "left-up"])
    }

    func testAPinchCancelsTheMoveWithoutClicking() {
        input.panBegan(at: CGPoint(x: 0.1, y: 0.1))
        input.panMoved(to: CGPoint(x: 0.11, y: 0.1), by: .zero)
        input.panCancelled()
        XCTAssertEqual(rec.buttons, [])
    }

    func testTrackpadModeMovesRelativelyBySpeed() {
        set {
            $0.mode = .trackpad
            $0.pointerSpeed = 2
        }
        input.panBegan(at: CGPoint(x: 0.9, y: 0.9))
        input.panMoved(to: CGPoint(x: 0.95, y: 0.9), by: CGPoint(x: 0.05, y: -0.1))
        input.panEnded()
        XCTAssertEqual(rec.buttons, [], "moving never presses a button")
        XCTAssertEqual(input.pointer.x, 0.6, accuracy: 0.0001)
        XCTAssertEqual(input.pointer.y, 0.3, accuracy: 0.0001)
    }

    func testNaturalScrollingFollowsTheFingersAndCanBeTurnedOff() {
        input.scroll(lines: CGPoint(x: 0, y: 1))
        set { $0.naturalScrolling = false }
        input.scroll(lines: CGPoint(x: 0, y: 1))
        XCTAssertEqual(rec.events, ["scroll 1.0", "scroll -1.0"])
    }

    func testBarButtonsHoldAndDontDoublePress() {
        input.bar(.left, pressed: true)
        // A touch-mode drag while the bar holds Left: no second press, no early release.
        input.panBegan(at: CGPoint(x: 0.1, y: 0.1))
        now += 1
        input.panMoved(to: CGPoint(x: 0.3, y: 0.3), by: .zero)
        input.panEnded()
        XCTAssertEqual(rec.buttons, ["left-down"])
        input.bar(.left, pressed: false)
        input.bar(.middle, pressed: true)
        input.bar(.middle, pressed: false)
        XCTAssertEqual(rec.buttons, ["left-down", "left-up", "middle-down", "middle-up"])
    }

    func testSwitchingModeLetsGoOfHeldButtons() {
        input.holdBegan(at: CGPoint(x: 0.2, y: 0.2))
        input.holdMoved(to: CGPoint(x: 0.4, y: 0.4), by: .zero)
        input.releaseAll()
        XCTAssertEqual(rec.buttons, ["left-down", "left-up"])
    }

    func testPointerStaysOnTheScreen() {
        input.tap(at: CGPoint(x: -0.5, y: 1.7))
        XCTAssertEqual(input.pointer, CGPoint(x: 0, y: 1))
    }

    func testGestureHelpFollowsTheSettings() {
        var s = InputSettings()
        XCTAssertEqual(GestureHelp.all(s).first?.action, "Left click where you tap")
        XCTAssertTrue(GestureHelp.all(s).contains { $0.gesture == "Two-finger tap" })
        s.mode = .trackpad
        s.twoFingerTapRightClick = false
        XCTAssertEqual(GestureHelp.all(s).first?.action, "Left click at the pointer")
        XCTAssertFalse(GestureHelp.all(s).contains { $0.gesture == "Two-finger tap" })
    }

    func testDefaults() {
        let s = InputSettings()
        XCTAssertEqual(s.mode, .touch)
        XCTAssertTrue(s.twoFingerTapRightClick)
        XCTAssertFalse(s.longPressRightClick)
        XCTAssertTrue(s.naturalScrolling)
        XCTAssertFalse(s.showMouseButtons)
        XCTAssertTrue(s.showPointer)
        XCTAssertEqual(s.pointerSpeed, 1)
    }
}
