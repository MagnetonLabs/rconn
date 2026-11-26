import AVFoundation
import UIKit

/// The computer's screen, driven like a trackpad: one finger moves the pointer, tap clicks,
/// two-finger tap right-clicks, press-and-hold then move drags, two fingers scroll, pinch
/// zooms the picture here (not on the computer). A hardware keyboard types straight through;
/// `showKeyboard()` brings up the soft keyboard with a modifier bar.
final class RemoteScreenView: UIView, UIGestureRecognizerDelegate {
    private let model: SessionModel
    private let keys = KeySender()
    private let videoView = UIView()
    private let cursor = UIImageView(image: UIImage(systemName: "cursorarrow"))
    private lazy var keyboard = KeyboardInputView(keys: keys)

    /// Pointer position as fractions of the remote screen.
    private var pointer = CGPoint(x: 0.5, y: 0.5)
    private var zoom: CGFloat = 1
    private var offset = CGPoint.zero
    private var pinchStart: (zoom: CGFloat, offset: CGPoint, center: CGPoint)?
    private var dragging = false
    private var scrollCarry = CGPoint.zero
    private var lastResize = CGSize.zero
    private var resizeWork: DispatchWorkItem?
    private weak var pinch: UIPinchGestureRecognizer?

    /// Points of finger travel per scrolled line (as the desktop viewer's trackpad scaling).
    private let pointsPerLine: CGFloat = 40

    init(model: SessionModel) {
        self.model = model
        super.init(frame: .zero)
        backgroundColor = .black
        clipsToBounds = true
        keys.session = model.session
        videoView.layer.addSublayer(model.renderer.layer)
        addSubview(videoView)
        cursor.tintColor = .white
        cursor.layer.shadowColor = UIColor.black.cgColor
        cursor.layer.shadowOpacity = 0.8
        cursor.layer.shadowRadius = 1.5
        cursor.layer.shadowOffset = .zero
        cursor.isHidden = true
        addSubview(cursor)
        addSubview(keyboard)
        installGestures()
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    private var session: Session? { model.session }

    // MARK: Layout

    override func layoutSubviews() {
        super.layoutSubviews()
        videoView.transform = .identity
        videoView.frame = bounds
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

    /// Ask the computer to fit its screen to this view's pixel size, once a rotation settles.
    private func sendResizeSoon() {
        let scale = window?.screen.scale ?? UIScreen.main.scale
        let px = CGSize(width: (bounds.width * scale / 2).rounded() * 2,
                        height: (bounds.height * scale / 2).rounded() * 2)
        guard px.width > 0, px.height > 0, px != lastResize else { return }
        lastResize = px
        resizeWork?.cancel()
        let work = DispatchWorkItem { [weak self] in
            self?.session?.sendResize(width: UInt32(px.width), height: UInt32(px.height))
        }
        resizeWork = work
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.4, execute: work)
    }

    /// The picture's rectangle inside `videoView` (before zoom).
    private var videoRect: CGRect {
        let size = model.videoSize
        guard size.width > 0, size.height > 0 else { return videoView.bounds }
        return AVMakeRect(aspectRatio: size, insideRect: videoView.bounds)
    }

    private func applyTransform() {
        videoView.transform = CGAffineTransform(translationX: offset.x, y: offset.y).scaledBy(x: zoom, y: zoom)
        placeCursor()
    }

    private func clampOffset() {
        let maxX = (zoom - 1) * bounds.width / 2
        let maxY = (zoom - 1) * bounds.height / 2
        offset.x = min(max(offset.x, -maxX), maxX)
        offset.y = min(max(offset.y, -maxY), maxY)
    }

    private func placeCursor() {
        let r = videoRect
        let local = CGPoint(x: r.minX + pointer.x * r.width, y: r.minY + pointer.y * r.height)
        let p = videoView.convert(local, to: self)
        // The arrow's tip is its top-left corner.
        cursor.frame = CGRect(x: p.x - 3, y: p.y - 2, width: 18, height: 22)
        cursor.isHidden = model.videoSize == .zero
    }

    /// When zoomed in, pan the picture so the pointer stays in view.
    private func followCursor() {
        guard zoom > 1 else { return }
        let margin: CGFloat = 40
        let c = cursor.frame.origin
        var dx: CGFloat = 0, dy: CGFloat = 0
        if c.x < margin { dx = margin - c.x } else if c.x > bounds.width - margin { dx = bounds.width - margin - c.x }
        if c.y < margin { dy = margin - c.y } else if c.y > bounds.height - margin { dy = bounds.height - margin - c.y }
        if dx != 0 || dy != 0 {
            offset.x += dx
            offset.y += dy
            clampOffset()
            applyTransform()
        }
    }

    // MARK: Pointer

    private func movePointer(byScreenDelta d: CGPoint) {
        let r = videoRect
        guard r.width > 0, r.height > 0 else { return }
        pointer.x = min(max(pointer.x + d.x / (r.width * zoom), 0), 1)
        pointer.y = min(max(pointer.y + d.y / (r.height * zoom), 0), 1)
        session?.sendPointer(x: Float(pointer.x), y: Float(pointer.y))
        placeCursor()
        followCursor()
    }

    /// Absolute positioning, for an iPad's mouse or trackpad pointer.
    private func pointTo(_ p: CGPoint) {
        let local = convert(p, to: videoView)
        let r = videoRect
        guard r.width > 0, r.height > 0 else { return }
        pointer.x = min(max((local.x - r.minX) / r.width, 0), 1)
        pointer.y = min(max((local.y - r.minY) / r.height, 0), 1)
        session?.sendPointer(x: Float(pointer.x), y: Float(pointer.y))
        placeCursor()
    }

    private func click(_ button: PointerButton) {
        session?.sendButton(button: button, pressed: true)
        session?.sendButton(button: button, pressed: false)
    }

    // MARK: Gestures

    private func installGestures() {
        let tap = UITapGestureRecognizer(target: self, action: #selector(onTap))
        let rightTap = UITapGestureRecognizer(target: self, action: #selector(onRightTap))
        rightTap.numberOfTouchesRequired = 2
        let secondaryClick = UITapGestureRecognizer(target: self, action: #selector(onRightTap))
        secondaryClick.buttonMaskRequired = .secondary
        let move = UIPanGestureRecognizer(target: self, action: #selector(onMove))
        move.maximumNumberOfTouches = 1
        let drag = UILongPressGestureRecognizer(target: self, action: #selector(onDrag))
        drag.minimumPressDuration = 0.35
        let scroll = UIPanGestureRecognizer(target: self, action: #selector(onScroll))
        scroll.minimumNumberOfTouches = 2
        scroll.maximumNumberOfTouches = 2
        scroll.allowedScrollTypesMask = .all // iPad trackpad two-finger scroll
        scroll.delegate = self
        let pinch = UIPinchGestureRecognizer(target: self, action: #selector(onPinch))
        pinch.delegate = self
        self.pinch = pinch
        let hover = UIHoverGestureRecognizer(target: self, action: #selector(onHover))
        for g in [tap, rightTap, secondaryClick, move, drag, scroll, pinch, hover] as [UIGestureRecognizer] {
            addGestureRecognizer(g)
        }
    }

    /// Pinch and two-finger scroll run together; scrolling pauses while the pinch zooms.
    func gestureRecognizer(_ g: UIGestureRecognizer,
                           shouldRecognizeSimultaneouslyWith other: UIGestureRecognizer) -> Bool {
        (g is UIPinchGestureRecognizer && other is UIPanGestureRecognizer)
            || (g is UIPanGestureRecognizer && other is UIPinchGestureRecognizer)
    }

    @objc private func onTap(_ g: UITapGestureRecognizer) {
        if g.state == .ended { click(.left) }
    }

    @objc private func onRightTap(_ g: UITapGestureRecognizer) {
        if g.state == .ended { click(.right) }
    }

    @objc private func onMove(_ g: UIPanGestureRecognizer) {
        guard g.state == .changed || g.state == .ended else { return }
        movePointer(byScreenDelta: g.translation(in: self))
        g.setTranslation(.zero, in: self)
    }

    private var dragLast = CGPoint.zero

    @objc private func onDrag(_ g: UILongPressGestureRecognizer) {
        let p = g.location(in: self)
        switch g.state {
        case .began:
            dragging = true
            dragLast = p
            UIImpactFeedbackGenerator(style: .light).impactOccurred()
            session?.sendButton(button: .left, pressed: true)
        case .changed:
            movePointer(byScreenDelta: CGPoint(x: p.x - dragLast.x, y: p.y - dragLast.y))
            dragLast = p
        case .ended, .cancelled, .failed:
            if dragging {
                dragging = false
                session?.sendButton(button: .left, pressed: false)
            }
        default:
            break
        }
    }

    @objc private func onScroll(_ g: UIPanGestureRecognizer) {
        if g.state == .began { scrollCarry = .zero }
        guard g.state == .changed else { return }
        let t = g.translation(in: self)
        g.setTranslation(.zero, in: self)
        if let pinch, pinch.state == .changed, abs(pinch.scale - 1) > 0.08 { return }
        // Natural scrolling: content follows the fingers.
        scrollCarry.x += t.x / pointsPerLine
        scrollCarry.y += t.y / pointsPerLine
        if abs(scrollCarry.x) >= 0.25 || abs(scrollCarry.y) >= 0.25 {
            session?.sendScroll(dx: Float(scrollCarry.x), dy: Float(scrollCarry.y))
            scrollCarry = .zero
        }
    }

    @objc private func onPinch(_ g: UIPinchGestureRecognizer) {
        switch g.state {
        case .began:
            pinchStart = (zoom, offset, g.location(in: self))
        case .changed:
            guard let start = pinchStart else { return }
            let newZoom = min(max(start.zoom * g.scale, 1), 5)
            // Keep the point under the fingers where it was.
            let mid = CGPoint(x: bounds.midX, y: bounds.midY)
            let c = start.center
            let k = newZoom / start.zoom
            offset.x = (c.x - mid.x) * (1 - k) + start.offset.x * k
            offset.y = (c.y - mid.y) * (1 - k) + start.offset.y * k
            zoom = newZoom
            clampOffset()
            applyTransform()
        default:
            pinchStart = nil
        }
    }

    @objc private func onHover(_ g: UIHoverGestureRecognizer) {
        if g.state == .began || g.state == .changed {
            pointTo(g.location(in: self))
        }
    }

    // MARK: Keyboard

    override var canBecomeFirstResponder: Bool { true }

    func showKeyboard() {
        keys.session = model.session
        if keyboard.isFirstResponder {
            _ = keyboard.resignFirstResponder()
            becomeFirstResponder()
        } else {
            keyboard.becomeFirstResponder()
        }
    }

    override func pressesBegan(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        keys.session = model.session
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
        placeCursor()
    }
}
