import UIKit

/// USB HID usages (page 7), as `rconn_core::hid::usage` and `UIKey.keyCode` use them.
enum HID {
    static let enter: UInt16 = 0x28
    static let escape: UInt16 = 0x29
    static let backspace: UInt16 = 0x2A
    static let tab: UInt16 = 0x2B
    static let space: UInt16 = 0x2C
    static let arrowRight: UInt16 = 0x4F
    static let arrowLeft: UInt16 = 0x50
    static let arrowDown: UInt16 = 0x51
    static let arrowUp: UInt16 = 0x52
    static let delete: UInt16 = 0x4C
    static let control: UInt16 = 0xE0
    static let shift: UInt16 = 0xE1
    static let alt: UInt16 = 0xE2
    static let meta: UInt16 = 0xE3

    /// The US-layout key for a character typed while a modifier is latched (Ctrl+C), and
    /// whether it needs Shift.
    static func key(for c: Character) -> (usage: UInt16, shift: Bool)? {
        guard let a = c.asciiValue else { return nil }
        switch c {
        case "a"..."z": return (0x04 + UInt16(a - 97), false)
        case "A"..."Z": return (0x04 + UInt16(a - 65), true)
        case "1"..."9": return (0x1E + UInt16(a - 49), false)
        case "0": return (0x27, false)
        case " ": return (space, false)
        case "-": return (0x2D, false)
        case "=": return (0x2E, false)
        case "[": return (0x2F, false)
        case "]": return (0x30, false)
        case "\\": return (0x31, false)
        case ";": return (0x33, false)
        case "'": return (0x34, false)
        case "`": return (0x35, false)
        case ",": return (0x36, false)
        case ".": return (0x37, false)
        case "/": return (0x38, false)
        default: return nil
        }
    }
}

/// Sends keys to the session, with modifiers latched from the modifier bar (the next key
/// is sent with them held, then they release).
final class KeySender {
    weak var session: Session?
    private(set) var latched: Set<UInt16> = []
    var onLatchChange: (() -> Void)?

    func toggle(_ modifier: UInt16) {
        if latched.contains(modifier) { latched.remove(modifier) } else { latched.insert(modifier) }
        onLatchChange?()
    }

    /// Press and release `usage` with the latched modifiers (plus Shift if asked).
    func tap(_ usage: UInt16, shift: Bool = false) {
        guard let session else { return }
        var mods = Array(latched)
        if shift && !latched.contains(HID.shift) { mods.append(HID.shift) }
        mods.forEach { session.sendKey(hidUsage: $0, pressed: true) }
        session.sendKey(hidUsage: usage, pressed: true)
        session.sendKey(hidUsage: usage, pressed: false)
        mods.reversed().forEach { session.sendKey(hidUsage: $0, pressed: false) }
        if !latched.isEmpty {
            latched.removeAll()
            onLatchChange?()
        }
    }

    /// Text from the soft keyboard: typed as text, unless a modifier is latched (then as
    /// key combinations, which shortcuts need).
    func type(_ text: String) {
        guard let session else { return }
        var plain = ""
        func flush() {
            if !plain.isEmpty { session.sendText(text: plain); plain = "" }
        }
        for c in text {
            if c == "\n" {
                flush()
                tap(HID.enter)
            } else if !latched.isEmpty, let k = HID.key(for: c) {
                flush()
                tap(k.usage, shift: k.shift)
            } else {
                plain.append(c)
            }
        }
        flush()
    }

    /// A hardware keyboard: physical keys straight through (`UIKey.keyCode` is a HID usage).
    /// Returns false for presses that aren't keys.
    func hardware(_ presses: Set<UIPress>, pressed: Bool) -> Bool {
        guard let session else { return false }
        var handled = false
        for p in presses {
            guard let key = p.key else { continue }
            session.sendKey(hidUsage: UInt16(truncatingIfNeeded: key.keyCode.rawValue), pressed: pressed)
            handled = true
        }
        return handled
    }
}

/// An invisible text-input view: first responder while the soft keyboard is up. Its
/// accessory view is the modifier bar.
final class KeyboardInputView: UIView, UIKeyInput {
    let keys: KeySender
    private lazy var bar = ModifierBar(keys: keys, onDone: { [weak self] in _ = self?.resignFirstResponder() })

    init(keys: KeySender) {
        self.keys = keys
        super.init(frame: CGRect(x: 0, y: 0, width: 1, height: 1))
        isHidden = false
        alpha = 0.01
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override var canBecomeFirstResponder: Bool { true }
    override var inputAccessoryView: UIView? { bar }

    // Plain keystrokes: no autocorrection, capitals or smart punctuation.
    var autocorrectionType: UITextAutocorrectionType = .no
    var autocapitalizationType: UITextAutocapitalizationType = .none
    var spellCheckingType: UITextSpellCheckingType = .no
    var smartQuotesType: UITextSmartQuotesType = .no
    var smartDashesType: UITextSmartDashesType = .no
    var smartInsertDeleteType: UITextSmartInsertDeleteType = .no
    var keyboardType: UIKeyboardType = .asciiCapable

    var hasText: Bool { true } // so Backspace always reaches deleteBackward
    func insertText(_ text: String) { keys.type(text) }
    func deleteBackward() { keys.tap(HID.backspace) }

    override func pressesBegan(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        if !keys.hardware(presses, pressed: true) { super.pressesBegan(presses, with: event) }
    }

    override func pressesEnded(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        if !keys.hardware(presses, pressed: false) { super.pressesEnded(presses, with: event) }
    }

    override func pressesCancelled(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        if !keys.hardware(presses, pressed: false) { super.pressesCancelled(presses, with: event) }
    }
}

/// Ctrl, Alt, Cmd/Win (latching), Esc, Tab, arrows, and a button to hide the keyboard.
final class ModifierBar: UIInputView {
    private let keys: KeySender
    private var modifierButtons: [(UInt16, UIButton)] = []

    init(keys: KeySender, onDone: @escaping () -> Void) {
        self.keys = keys
        super.init(frame: CGRect(x: 0, y: 0, width: 320, height: 48), inputViewStyle: .keyboard)
        autoresizingMask = .flexibleWidth

        let stack = UIStackView()
        stack.axis = .horizontal
        stack.spacing = 6
        stack.translatesAutoresizingMaskIntoConstraints = false
        let scroll = UIScrollView()
        scroll.showsHorizontalScrollIndicator = false
        scroll.translatesAutoresizingMaskIntoConstraints = false
        scroll.addSubview(stack)
        addSubview(scroll)

        for (title, usage) in [("ctrl", HID.control), ("alt", HID.alt), ("⌘/win", HID.meta), ("shift", HID.shift)] {
            let b = Self.button(title) { [weak keys] in keys?.toggle(usage) }
            modifierButtons.append((usage, b))
            stack.addArrangedSubview(b)
        }
        for (title, usage) in [("esc", HID.escape), ("tab", HID.tab), ("←", HID.arrowLeft), ("↑", HID.arrowUp),
                               ("↓", HID.arrowDown), ("→", HID.arrowRight), ("del", HID.delete)] {
            stack.addArrangedSubview(Self.button(title) { [weak keys] in keys?.tap(usage) })
        }
        let done = Self.button("⌨︎↓", action: onDone)
        done.accessibilityLabel = "Hide keyboard"
        done.translatesAutoresizingMaskIntoConstraints = false
        addSubview(done)

        NSLayoutConstraint.activate([
            scroll.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 8),
            scroll.trailingAnchor.constraint(equalTo: done.leadingAnchor, constant: -8),
            scroll.topAnchor.constraint(equalTo: topAnchor),
            scroll.bottomAnchor.constraint(equalTo: bottomAnchor),
            stack.leadingAnchor.constraint(equalTo: scroll.contentLayoutGuide.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: scroll.contentLayoutGuide.trailingAnchor),
            stack.centerYAnchor.constraint(equalTo: scroll.centerYAnchor),
            stack.heightAnchor.constraint(equalToConstant: 36),
            done.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -8),
            done.centerYAnchor.constraint(equalTo: centerYAnchor),
        ])
        keys.onLatchChange = { [weak self] in self?.refresh() }
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    private func refresh() {
        for (usage, b) in modifierButtons {
            b.isSelected = keys.latched.contains(usage)
        }
    }

    private static func button(_ title: String, action: @escaping () -> Void) -> UIButton {
        var config = UIButton.Configuration.gray()
        config.title = title
        config.cornerStyle = .medium
        config.contentInsets = NSDirectionalEdgeInsets(top: 6, leading: 10, bottom: 6, trailing: 10)
        let b = UIButton(configuration: config, primaryAction: UIAction { _ in action() })
        b.configurationUpdateHandler = { b in
            var c = b.configuration
            c?.baseBackgroundColor = b.isSelected ? .tintColor : nil
            c?.baseForegroundColor = b.isSelected ? .white : nil
            b.configuration = c
        }
        return b
    }
}
