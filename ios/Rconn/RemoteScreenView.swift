import AVFoundation
import UIKit

/// The computer's screen. Touch drives the pointer by the input settings (`PointerInput`):
/// Touch mode clicks and drags where the finger is, Trackpad mode moves the pointer like a
/// laptop's trackpad. Two fingers scroll, pinch zooms the picture here (not on the
/// computer). A hardware keyboard types straight through; `showKeyboard()` brings up the
/// soft keyboard with a modifier bar.
///
/// The picture is centred in the area below the toolbar (`reserve`).
final class RemoteScreenView: UIView, UIGestureRecognizerDelegate {
    private let model: SessionModel
    let input = PointerInput()
    private let keys = KeySender()
    /// The area the picture fits in (the view minus `contentInsets`); zoom transforms it.
    private let videoView = UIView()
    private let cursor = UIImageView(image: UIImage(systemName: "cursorarrow"))
    private lazy var keyboard = KeyboardInputView(keys: keys)

    /// Room kept free for the toolbar above (and the mouse buttons bar below, from
    /// `bottomEdge` down): the picture is centred in what is left.
    private var reservedTop: CGFloat = 0
    private var bottomEdge: CGFloat?

    private var zoom: CGFloat = 1
    private var offset = CGPoint.zero
    private var pinchStart: (zoom: CGFloat, offset: CGPoint, center: CGPoint)?
    private var holdLast = CGPoint.zero
    private var holdStart = CGPoint.zero
    private var holdMoving = false
    private var lastResize = CGSize.zero
    private var resizeWork: DispatchWorkItem?
    private weak var pinch: UIPinchGestureRecognizer?
    private weak var move: UIPanGestureRecognizer?
    private weak var hold: UILongPressGestureRecognizer?

    /// Points of finger travel per scrolled line (as the desktop viewer's trackpad scaling).
    static let pointsPerLine: CGFloat = 40
    /// A held finger moving less than this still clicks rather than drags.
    private let holdSlop: CGFloat = 8

    init(model: SessionModel) {
        self.model = model
        super.init(frame: .zero)
        backgroundColor = .black
        clipsToBounds = true
        input.sink = { [weak model] in model?.session }
        input.onPointerMoved = { [weak self] in
            self?.placeCursor()
            self?.followCursor()
        }
        keys.currentSession = { [weak model] in model?.session }
        videoView.layer.addSublayer(model.renderer.layer)
        addSubview(videoView)
        cursor.tintColor = .white
        cursor.layer.shadowColor = UIColor.black.cgColor
        cursor.layer.shadowOpacity = 0.8
        cursor.layer.shadowRadius = 1.5
        cursor.layer.shadowOffset = .zero
        cursor.isHidden = true
        // Tests find the pointer by this (not an element for VoiceOver otherwise).
        cursor.isAccessibilityElement = TestProbe.shared.enabled
        cursor.accessibilityIdentifier = "remotePointer"
        addSubview(cursor)
        addSubview(keyboard)
        installGestures()
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    /// New settings (applied at once, mid-session too).
    func apply(_ settings: InputSettings) {
        guard settings != input.settings else { return }
        if settings.mode != input.settings.mode {
            input.releaseAll()
        }
        input.settings = settings
        placeCursor()
    }

    // MARK: Layout

    func reserve(top: CGFloat, bottomEdge: CGFloat?) {
        guard top != reservedTop || bottomEdge != self.bottomEdge else { return }
        reservedTop = top
        self.bottomEdge = bottomEdge
        setNeedsLayout()
    }

    /// The area the picture fits in, in this view's coordinates (before zoom).
    private var area: CGRect {
        let top = min(max(reservedTop, 0), bounds.height / 2)
        let bottom = min(bottomEdge ?? bounds.height, bounds.height)
        return CGRect(x: 0, y: top, width: bounds.width, height: max(bottom - top, bounds.height / 3))
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        videoView.transform = .identity
        videoView.frame = area
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        model.renderer.layer.frame = videoView.bounds
        CATransaction.commit()
        clampOffset()
        applyTransform()
        sendResizeSoon()
    }

    override func didMoveToWindow() {
        super.didMoveToWindow()
        if window != nil {
            becomeFirstResponder() // hardware keyboard
        }
    }

    /// Ask the computer to fit its screen to the picture area's pixel size, once a rotation
    /// settles.
    private func sendResizeSoon() {
        let scale = window?.screen.scale ?? UIScreen.main.scale
        let a = area
        let px = CGSize(width: (a.width * scale / 2).rounded() * 2,
                        height: (a.height * scale / 2).rounded() * 2)
        guard px.width > 0, px.height > 0, px != lastResize else { return }
        lastResize = px
        resizeWork?.cancel()
        let work = DispatchWorkItem { [weak self] in
            self?.model.session?.sendResize(width: UInt32(px.width), height: UInt32(px.height))
        }
        resizeWork = work
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.4, execute: work)
    }

    /// A new session (after a reconnect) needs the size again.
    func sessionReplaced() {
        lastResize = .zero
        sendResizeSoon()
    }

    /// The picture's rectangle inside `videoView` (before zoom): centred, aspect kept.
    private var videoRect: CGRect {
        let size = model.videoSize
        guard size.width > 0, size.height > 0 else { return videoView.bounds }
        return AVMakeRect(aspectRatio: size, insideRect: videoView.bounds)
    }

    /// The picture's rectangle on screen now (zoom included).
    var pictureFrame: CGRect {
        videoView.convert(videoRect, to: self)
    }

    private func applyTransform() {
        videoView.transform = CGAffineTransform(translationX: offset.x, y: offset.y).scaledBy(x: zoom, y: zoom)
        placeCursor()
        if TestProbe.shared.enabled {
            let f = pictureFrame
            DispatchQueue.main.async { TestProbe.shared.pictureFrame = f }
        }
    }

    private func clampOffset() {
        let maxX = (zoom - 1) * area.width / 2
        let maxY = (zoom - 1) * area.height / 2
        offset.x = min(max(offset.x, -maxX), maxX)
        offset.y = min(max(offset.y, -maxY), maxY)
    }

    private func placeCursor() {
        let r = videoRect
        let p = input.pointer
        let local = CGPoint(x: r.minX + p.x * r.width, y: r.minY + p.y * r.height)
        let at = videoView.convert(local, to: self)
        // The arrow's tip is its top-left corner.
        cursor.frame = CGRect(x: at.x - 3, y: at.y - 2, width: 18, height: 22)
        cursor.isHidden = model.videoSize == .zero || !input.settings.showPointer
    }

    /// Trackpad mode, zoomed in: pan the picture so the pointer stays in view.
    private func followCursor() {
        guard zoom > 1, input.settings.mode == .trackpad else { return }
        let margin: CGFloat = 40
        let a = area
        let c = cursor.frame.origin
        var dx: CGFloat = 0, dy: CGFloat = 0
        if c.x < a.minX + margin { dx = a.minX + margin - c.x } else if c.x > a.maxX - margin { dx = a.maxX - margin - c.x }
        if c.y < a.minY + margin { dy = a.minY + margin - c.y } else if c.y > a.maxY - margin { dy = a.maxY - margin - c.y }
        if dx != 0 || dy != 0 {
            offset.x += dx
            offset.y += dy
            clampOffset()
            applyTransform()
        }
    }

    // MARK: Geometry

    /// A point in this view as a fraction of the remote screen (may lie outside 0…1).
    private func remote(_ p: CGPoint) -> CGPoint {
        let local = convert(p, to: videoView)
        let r = videoRect
        guard r.width > 0, r.height > 0 else { return CGPoint(x: 0.5, y: 0.5) }
        return CGPoint(x: (local.x - r.minX) / r.width, y: (local.y - r.minY) / r.height)
    }

    /// Finger travel on screen as a fraction of the (zoomed) picture.
    private func remoteDelta(_ d: CGPoint) -> CGPoint {
        let r = videoRect
        guard r.width > 0, r.height > 0 else { return .zero }
        return CGPoint(x: d.x / (r.width * zoom), y: d.y / (r.height * zoom))
    }

    // MARK: Gestures

    private func installGestures() {
        // One-finger tap: always the left button.
        let tap = UITapGestureRecognizer(target: self, action: #selector(onTap))
        tap.allowedTouchTypes = [UITouch.TouchType.direct, .pencil, .indirectPointer]
            .map { NSNumber(value: $0.rawValue) }
        let rightTap = UITapGestureRecognizer(target: self, action: #selector(onTwoFingerTap))
        rightTap.numberOfTouchesRequired = 2
        rightTap.allowedTouchTypes = [NSNumber(value: UITouch.TouchType.direct.rawValue)]
        // A mouse's or trackpad's right button (iPad). Without the touch-type limit this
        // recognizer also took plain finger taps (the button mask only applies to pointer
        // input) and turned them into right clicks.
        let secondaryClick = UITapGestureRecognizer(target: self, action: #selector(onSecondaryClick))
        secondaryClick.buttonMaskRequired = .secondary
        secondaryClick.allowedTouchTypes = [NSNumber(value: UITouch.TouchType.indirectPointer.rawValue)]
        let move = UIPanGestureRecognizer(target: self, action: #selector(onMove))
        move.maximumNumberOfTouches = 1
        move.delegate = self
        self.move = move
        let hold = UILongPressGestureRecognizer(target: self, action: #selector(onHold))
        hold.minimumPressDuration = 0.45
        self.hold = hold
        let scroll = UIPanGestureRecognizer(target: self, action: #selector(onScroll))
        scroll.minimumNumberOfTouches = 2
        scroll.maximumNumberOfTouches = 2
        scroll.allowedScrollTypesMask = .all // iPad trackpad two-finger scroll
        scroll.delegate = self
        let pinch = UIPinchGestureRecognizer(target: self, action: #selector(onPinch))
        pinch.delegate = self
        self.pinch = pinch
        let hover = UIHoverGestureRecognizer(target: self, action: #selector(onHover))
        for g in [tap, rightTap, secondaryClick, move, hold, scroll, pinch, hover] as [UIGestureRecognizer] {
            addGestureRecognizer(g)
        }
    }

    /// Pinch runs with two-finger scroll (scrolling pauses while it zooms) and with the
    /// one-finger move (which it then cancels: the first finger of a pinch isn't a drag).
    func gestureRecognizer(_ g: UIGestureRecognizer,
                           shouldRecognizeSimultaneouslyWith other: UIGestureRecognizer) -> Bool {
        (g is UIPinchGestureRecognizer && other is UIPanGestureRecognizer)
            || (g is UIPanGestureRecognizer && other is UIPinchGestureRecognizer)
    }

    @objc private func onTap(_ g: UITapGestureRecognizer) {
        guard g.state == .ended else { return }
        input.tap(at: remote(g.location(in: self)))
    }

    @objc private func onTwoFingerTap(_ g: UITapGestureRecognizer) {
        guard g.state == .ended else { return }
        input.twoFingerTap(at: remote(g.location(in: self)))
    }

    @objc private func onSecondaryClick(_ g: UITapGestureRecognizer) {
        guard g.state == .ended else { return }
        input.secondaryClick(at: remote(g.location(in: self)))
    }

    @objc private func onMove(_ g: UIPanGestureRecognizer) {
        let p = g.location(in: self)
        switch g.state {
        case .began:
            // Where the finger went down (the pan starts after some travel).
            let t = g.translation(in: self)
            input.panBegan(at: remote(CGPoint(x: p.x - t.x, y: p.y - t.y)))
            input.panMoved(to: remote(p), by: remoteDelta(t))
            g.setTranslation(.zero, in: self)
        case .changed:
            input.panMoved(to: remote(p), by: remoteDelta(g.translation(in: self)))
            g.setTranslation(.zero, in: self)
        case .ended:
            input.panMoved(to: remote(p), by: remoteDelta(g.translation(in: self)))
            input.panEnded()
        case .cancelled, .failed:
            input.panCancelled()
        default:
            break
        }
    }

    @objc private func onHold(_ g: UILongPressGestureRecognizer) {
        let p = g.location(in: self)
        switch g.state {
        case .began:
            holdStart = p
            holdLast = p
            holdMoving = false
            UIImpactFeedbackGenerator(style: input.settings.longPressRightClick ? .medium : .light).impactOccurred()
            input.holdBegan(at: remote(p))
        case .changed:
            if !holdMoving, hypot(p.x - holdStart.x, p.y - holdStart.y) < holdSlop { return }
            holdMoving = true
            input.holdMoved(to: remote(p), by: remoteDelta(CGPoint(x: p.x - holdLast.x, y: p.y - holdLast.y)))
            holdLast = p
        case .ended:
            input.holdEnded()
        case .cancelled, .failed:
            input.holdCancelled()
        default:
            break
        }
    }

    @objc private func onScroll(_ g: UIPanGestureRecognizer) {
        if g.state == .ended || g.state == .cancelled { input.scrollEnded() }
        guard g.state == .changed else { return }
        let t = g.translation(in: self)
        g.setTranslation(.zero, in: self)
        if let pinch, pinch.state == .changed, abs(pinch.scale - 1) > 0.08 { return }
        input.scroll(lines: CGPoint(x: t.x / Self.pointsPerLine, y: t.y / Self.pointsPerLine))
    }

    @objc private func onPinch(_ g: UIPinchGestureRecognizer) {
        switch g.state {
        case .began:
            pinchStart = (zoom, offset, g.location(in: self))
            // The pinch's first finger may have started a one-finger move: not a drag.
            if let move, move.state == .began || move.state == .changed {
                move.isEnabled = false
                move.isEnabled = true
            }
        case .changed:
            guard let start = pinchStart, g.numberOfTouches >= 2 else { return }
            let newZoom = min(max(start.zoom * g.scale, 1), 5)
            // Keep the point that was under the fingers under them, wherever they move.
            let a = area
            let mid = CGPoint(x: a.midX, y: a.midY)
            let c0 = start.center
            let c = g.location(in: self)
            let k = newZoom / start.zoom
            offset.x = (c.x - mid.x) - k * (c0.x - mid.x) + k * start.offset.x
            offset.y = (c.y - mid.y) - k * (c0.y - mid.y) + k * start.offset.y
            zoom = newZoom
            clampOffset()
            applyTransform()
        default:
            pinchStart = nil
        }
    }

    @objc private func onHover(_ g: UIHoverGestureRecognizer) {
        if g.state == .began || g.state == .changed {
            input.hover(to: remote(g.location(in: self)))
        }
    }

    // MARK: Keyboard

    override var canBecomeFirstResponder: Bool { true }

    func showKeyboard() {
        if keyboard.isFirstResponder {
            _ = keyboard.resignFirstResponder()
            becomeFirstResponder()
        } else {
            keyboard.becomeFirstResponder()
        }
    }

    override func pressesBegan(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        if !keys.hardware(presses, pressed: true) { super.pressesBegan(presses, with: event) }
    }

    override func pressesEnded(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        if !keys.hardware(presses, pressed: false) { super.pressesEnded(presses, with: event) }
    }

    override func pressesCancelled(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        if !keys.hardware(presses, pressed: false) { super.pressesCancelled(presses, with: event) }
    }

    /// The first picture (or a new size) arrived.
    func videoSizeChanged() {
        applyTransform()
    }
}
