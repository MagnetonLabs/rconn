import SwiftUI

/// How one finger drives the computer's pointer.
enum PointerMode: String, CaseIterable, Identifiable {
    /// The screen is the computer's screen: tap clicks where you tap, a finger drags.
    case touch
    /// The screen is a trackpad: a finger moves the pointer, tap clicks where it is.
    case trackpad

    var id: String { rawValue }
    var title: String { self == .touch ? "Touch" : "Trackpad" }
}

/// Touch input settings, kept for the app (every computer) and applied at once.
struct InputSettings: Equatable {
    var mode: PointerMode = .touch
    var twoFingerTapRightClick = true
    var longPressRightClick = false
    /// Trackpad mode: pointer travel per finger travel (1: the picture follows the finger).
    var pointerSpeed: Double = 1
    /// Content follows the fingers, as on iPhone and iPad.
    var naturalScrolling = true
    var showMouseButtons = false
    var showPointer = true

    enum Key {
        static let mode = "input.mode"
        static let twoFingerTap = "input.twoFingerTapRightClick"
        static let longPress = "input.longPressRightClick"
        static let speed = "input.pointerSpeed"
        static let natural = "input.naturalScrolling"
        static let mouseButtons = "input.showMouseButtons"
        static let pointer = "input.showPointer"
    }

    static let speedRange: ClosedRange<Double> = 0.5...3
}

/// The settings as `@AppStorage`, for SwiftUI views: changes show everywhere at once.
struct StoredInputSettings: DynamicProperty {
    @AppStorage(InputSettings.Key.mode) var mode = InputSettings().mode
    @AppStorage(InputSettings.Key.twoFingerTap) var twoFingerTapRightClick = InputSettings().twoFingerTapRightClick
    @AppStorage(InputSettings.Key.longPress) var longPressRightClick = InputSettings().longPressRightClick
    @AppStorage(InputSettings.Key.speed) var pointerSpeed = InputSettings().pointerSpeed
    @AppStorage(InputSettings.Key.natural) var naturalScrolling = InputSettings().naturalScrolling
    @AppStorage(InputSettings.Key.mouseButtons) var showMouseButtons = InputSettings().showMouseButtons
    @AppStorage(InputSettings.Key.pointer) var showPointer = InputSettings().showPointer

    var value: InputSettings {
        InputSettings(
            mode: mode, twoFingerTapRightClick: twoFingerTapRightClick,
            longPressRightClick: longPressRightClick,
            pointerSpeed: min(max(pointerSpeed, InputSettings.speedRange.lowerBound), InputSettings.speedRange.upperBound),
            naturalScrolling: naturalScrolling, showMouseButtons: showMouseButtons, showPointer: showPointer)
    }
}

/// The Input settings sheet (session toolbar and connect screen).
struct InputSettingsView: View {
    private var s = StoredInputSettings()
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    Picker("Mode", selection: s.$mode) {
                        ForEach(PointerMode.allCases) { Text($0.title).tag($0) }
                    }
                    .pickerStyle(.segmented)
                    .accessibilityIdentifier("inputMode")
                } header: {
                    Text("Pointer")
                } footer: {
                    Text(s.mode == .touch
                         ? "Tap clicks where you tap; drag with one finger to drag (select, move windows)."
                         : "Move one finger to move the pointer, like a laptop's trackpad; tap clicks where the pointer is.")
                }

                Section {
                    Toggle("Two-finger tap", isOn: s.$twoFingerTapRightClick)
                        .accessibilityIdentifier("twoFingerTapRightClick")
                    Toggle("Touch and hold", isOn: s.$longPressRightClick)
                        .accessibilityIdentifier("longPressRightClick")
                } header: {
                    Text("Right click")
                } footer: {
                    if !s.twoFingerTapRightClick && !s.longPressRightClick {
                        Text("Right click with the mouse buttons (below).")
                    } else {
                        Text("A one-finger tap is always a left click.")
                    }
                }

                Section {
                    HStack {
                        Image(systemName: "tortoise")
                        Slider(value: s.$pointerSpeed, in: InputSettings.speedRange, step: 0.25)
                            .accessibilityIdentifier("pointerSpeed")
                            .accessibilityValue(String(format: "%.2g×", s.pointerSpeed))
                        Image(systemName: "hare")
                    }
                    .disabled(s.mode != .trackpad)
                    Toggle("Natural scrolling", isOn: s.$naturalScrolling)
                        .accessibilityIdentifier("naturalScrolling")
                } header: {
                    Text("Pointer speed and scrolling")
                } footer: {
                    Text(s.mode == .trackpad ? "Natural scrolling: the content follows your fingers."
                                             : "Pointer speed is for Trackpad mode. Natural scrolling: the content follows your fingers.")
                }

                Section("Show") {
                    Toggle("Mouse buttons", isOn: s.$showMouseButtons)
                        .accessibilityIdentifier("showMouseButtons")
                    Toggle("Pointer", isOn: s.$showPointer)
                        .accessibilityIdentifier("showPointer")
                }

                Section("Gestures") {
                    ForEach(GestureHelp.all(s.value), id: \.gesture) { g in
                        LabeledContent(g.gesture, value: g.action)
                            .accessibilityElement(children: .ignore)
                            .accessibilityLabel(g.gesture)
                            .accessibilityValue(g.action)
                            .accessibilityIdentifier("gesture-\(g.gesture)")
                    }
                }
            }
            .navigationTitle("Input")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") { dismiss() }
                        .accessibilityIdentifier("inputDone")
                }
            }
        }
    }
}

/// One row of the Gestures help: what a gesture does with the current settings.
struct GestureHelp: Equatable {
    let gesture: String
    let action: String

    static func all(_ s: InputSettings) -> [GestureHelp] {
        let touch = s.mode == .touch
        let hold: String
        if s.longPressRightClick {
            hold = "Right click (move after holding: drag)"
        } else {
            hold = "Left click (move after holding: drag)"
        }
        var rows = [
            GestureHelp(gesture: "Tap", action: touch ? "Left click where you tap" : "Left click at the pointer"),
            GestureHelp(gesture: "One finger", action: touch ? "Drag (left button held)" : "Move the pointer"),
            GestureHelp(gesture: "Touch and hold", action: hold),
        ]
        if s.twoFingerTapRightClick {
            rows.append(GestureHelp(gesture: "Two-finger tap", action: "Right click"))
        }
        rows += [
            GestureHelp(gesture: "Two fingers", action: "Scroll"),
            GestureHelp(gesture: "Pinch", action: "Zoom the picture (move while pinching to pan)"),
        ]
        if !touch {
            rows.append(GestureHelp(gesture: "Zoomed in", action: "The picture follows the pointer"))
        }
        return rows
    }
}
