import CoreGraphics
import QuartzCore

/// Where pointer input goes: the session (or a test's recorder).
protocol InputSink: AnyObject {
    func sendPointer(x: Float, y: Float)
    func sendButton(button: PointerButton, pressed: Bool)
    func sendScroll(dx: Float, dy: Float)
}

extension Session: InputSink {}

/// Turns gestures into the computer's pointer events, by the input settings. Positions are
/// fractions (0…1) of the remote screen; the view does the geometry. No UIKit here, so it
/// is unit-tested as it is.
///
/// The one rule that matters most: a one-finger tap is a left click, never anything else.
final class PointerInput {
    var settings = InputSettings()
    var sink: () -> InputSink? = { nil }
    /// Test hook for the touch-mode drag delay.
    var clock: () -> CFTimeInterval = CACurrentMediaTime
    /// The pointer moved (to redraw the cursor).
    var onPointerMoved: (() -> Void)?

    /// Touch mode: a one-finger drag presses the button only after this long, so a finger
    /// that becomes a pinch doesn't drag first.
    static let dragDelay: CFTimeInterval = 0.12

    private(set) var pointer = CGPoint(x: 0.5, y: 0.5)
    /// Buttons held by gestures and by the mouse buttons bar; the computer sees one press
    /// while either holds it.
    private var gestureHeld: Set<PointerButton> = []
    private var barHeld: Set<PointerButton> = []

    private var pan: (start: CGPoint, began: CFTimeInterval, last: CGPoint)?
    private var hold: (start: CGPoint, dragging: Bool)?
    private var scrollCarry = CGPoint.zero

    private var touch: Bool { settings.mode == .touch }

    // MARK: Taps

    /// One finger tapped at `p`: a left click (where you tap in touch mode, at the pointer
    /// in trackpad mode).
    func tap(at p: CGPoint) {
        if touch { move(to: p) }
        click(.left)
    }

    /// Two fingers tapped (centred at `p`): a right click, if that's on.
    func twoFingerTap(at p: CGPoint) {
        guard settings.twoFingerTapRightClick else { return }
        if touch { move(to: p) }
        click(.right)
    }

    /// A mouse's or trackpad's secondary button (iPad), at the pointer `p`.
    func secondaryClick(at p: CGPoint) {
        move(to: p)
        click(.right)
    }

    /// An iPad's mouse or trackpad pointer hovering at `p`.
    func hover(to p: CGPoint) {
        move(to: p)
    }

    // MARK: One-finger move

    /// A finger started moving; it went down at `start`.
    func panBegan(at start: CGPoint) {
        pan = (start, clock(), start)
        if touch { move(to: start) }
    }

    /// The finger is at `p`, `delta` from the last call (fractions of the picture).
    func panMoved(to p: CGPoint, by delta: CGPoint) {
        guard var current = pan else { return }
        current.last = p
        pan = current
        if touch {
            if !gestureHeld.contains(.left) {
                guard clock() - current.began >= Self.dragDelay else { return }
                press(.left)
            }
            move(to: p)
        } else {
            moveBy(delta)
        }
    }

    func panEnded() {
        guard let current = pan else { return }
        pan = nil
        guard touch else { return }
        if !gestureHeld.contains(.left) {
            // A quick flick: still a drag from where it started to where it ended.
            press(.left)
            move(to: current.last)
        }
        release(.left)
    }

    /// The move became something else (a pinch): let go without the quick-flick drag.
    func panCancelled() {
        pan = nil
        if touch { release(.left) }
    }

    // MARK: Touch and hold

    /// A finger was held still at `p`.
    func holdBegan(at p: CGPoint) {
        hold = (p, false)
        if touch { move(to: p) }
    }

    /// The held finger moved (past the view's slop): a drag with the left button.
    func holdMoved(to p: CGPoint, by delta: CGPoint) {
        guard var h = hold else { return }
        if !h.dragging {
            h.dragging = true
            hold = h
            press(.left)
        }
        if touch { move(to: p) } else { moveBy(delta) }
    }

    /// Let go: the end of a drag, or a click (right if touch-and-hold right-clicks).
    func holdEnded() {
        guard let h = hold else { return }
        hold = nil
        if h.dragging {
            release(.left)
        } else {
            click(settings.longPressRightClick ? .right : .left)
        }
    }

    func holdCancelled() {
        guard let h = hold else { return }
        hold = nil
        if h.dragging { release(.left) }
    }

    // MARK: Scrolling

    /// Two fingers moved by `lines` (finger travel in lines; down and right positive).
    func scroll(lines: CGPoint) {
        let sign: CGFloat = settings.naturalScrolling ? 1 : -1
        scrollCarry.x += lines.x * sign
        scrollCarry.y += lines.y * sign
        if abs(scrollCarry.x) >= 0.25 || abs(scrollCarry.y) >= 0.25 {
            sink()?.sendScroll(dx: Float(scrollCarry.x), dy: Float(scrollCarry.y))
            TestProbe.shared.scroll()
            scrollCarry = .zero
        }
    }

    func scrollEnded() {
        scrollCarry = .zero
    }

    // MARK: Mouse buttons bar

    /// A bar button went down or up (held as long as the finger stays on it).
    func bar(_ b: PointerButton, pressed: Bool) {
        let was = isDown(b)
        if pressed { barHeld.insert(b) } else { barHeld.remove(b) }
        sendIfChanged(b, was: was)
    }

    // MARK: Pointer

    /// Trackpad mode: move by `delta` (fractions of the picture) times the pointer speed.
    func moveBy(_ delta: CGPoint) {
        let k = settings.pointerSpeed
        move(to: CGPoint(x: pointer.x + delta.x * k, y: pointer.y + delta.y * k))
    }

    func move(to p: CGPoint) {
        let clamped = CGPoint(x: min(max(p.x, 0), 1), y: min(max(p.y, 0), 1))
        pointer = clamped
        sink()?.sendPointer(x: Float(clamped.x), y: Float(clamped.y))
        onPointerMoved?()
    }

    /// Let go of everything gestures hold (the session is going away or settings changed).
    func releaseAll() {
        pan = nil
        hold = nil
        for b in gestureHeld { release(b) }
        for b in barHeld { bar(b, pressed: false) }
    }

    private func isDown(_ b: PointerButton) -> Bool {
        gestureHeld.contains(b) || barHeld.contains(b)
    }

    private func press(_ b: PointerButton) {
        let was = isDown(b)
        gestureHeld.insert(b)
        sendIfChanged(b, was: was)
    }

    private func release(_ b: PointerButton) {
        let was = isDown(b)
        gestureHeld.remove(b)
        sendIfChanged(b, was: was)
    }

    private func click(_ b: PointerButton) {
        // A bar button already holding it: the click is the bar's to end.
        guard !barHeld.contains(b) else { return }
        press(b)
        release(b)
    }

    private func sendIfChanged(_ b: PointerButton, was: Bool) {
        let now = isDown(b)
        guard now != was else { return }
        sink()?.sendButton(button: b, pressed: now)
        TestProbe.shared.button(b, pressed: now)
    }
}
