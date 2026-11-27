import SwiftUI

/// Full-screen session: the remote screen centred below a small toolbar (keyboard, Keys
/// menu, sound, files, paste, input settings, disconnect), a status line until the picture
/// shows (or while reconnecting), and optionally the mouse buttons bar.
struct SessionView: View {
    @ObservedObject var model: SessionModel
    let onClose: (String) -> Void
    @State private var screen: RemoteScreenView?
    @State private var showingFiles = false
    @State private var showingSettings = false
    /// Where the toolbar ends and the mouse buttons bar starts (screen points): the
    /// picture is centred between them.
    @State private var toolbarBottom: CGFloat = 0
    @State private var barTop: CGFloat?
    private var settings = StoredInputSettings()

    init(model: SessionModel, onClose: @escaping (String) -> Void) {
        self.model = model
        self.onClose = onClose
    }

    var body: some View {
        ZStack(alignment: .top) {
            RemoteScreen(model: model, screen: $screen, settings: settings.value,
                         top: toolbarBottom, barTop: settings.showMouseButtons ? barTop : nil)
                .ignoresSafeArea()

            if let reason = model.gaveUp {
                gaveUpMessage(reason)
                    .frame(maxHeight: .infinity)
            } else if !model.status.isEmpty {
                VStack(spacing: 12) {
                    ProgressView().tint(.white)
                    if let r = model.reconnecting {
                        Text(r)
                            .font(.headline)
                            .foregroundStyle(.white)
                            .accessibilityIdentifier("reconnecting")
                    }
                    Text(model.status)
                        .foregroundStyle(.white)
                        .multilineTextAlignment(.center)
                        .accessibilityIdentifier("status")
                }
                .padding(20)
                .background(.black.opacity(0.6), in: RoundedRectangle(cornerRadius: 12))
                .frame(maxHeight: .infinity)
            }

            TestProbeLabel()

            toolbar
                .onGeometryChange(for: CGFloat.self) { $0.frame(in: .global).maxY } action: { y in
                    toolbarBottom = y + 4
                }

            if settings.showMouseButtons {
                MouseButtonsBar(input: screen?.input)
                    .onGeometryChange(for: CGFloat.self) { $0.frame(in: .global).minY } action: { y in
                        barTop = y - 6
                    }
                    .frame(maxHeight: .infinity, alignment: .bottom)
                    .padding(.bottom, 6)
            }
        }
        .background(Color.black)
        .ignoresSafeArea(.keyboard) // the soft keyboard covers the picture; no resize
        .statusBarHidden()
        .persistentSystemOverlays(.hidden)
        .onChange(of: model.videoSize) { _, _ in screen?.videoSizeChanged() }
        .onChange(of: model.generation) { _, _ in screen?.sessionReplaced() }
        .onChange(of: model.closedReason) { _, reason in
            if let reason { onClose(reason) }
        }
        .sheet(isPresented: $showingFiles) {
            FilesView(files: model.files)
        }
        .sheet(isPresented: $showingSettings) {
            InputSettingsView()
        }
    }

    private var toolbar: some View {
        HStack(spacing: 8) {
            Text(model.computerID)
                .font(.caption.monospacedDigit())
                .foregroundStyle(.white.opacity(0.8))
                .lineLimit(1)
                .minimumScaleFactor(0.7)
            Spacer(minLength: 4)
            toolbarButton("keyboard", label: "Keyboard") { screen?.showKeyboard() }
            Menu {
                ForEach(Array(model.combos.enumerated()), id: \.offset) { i, combo in
                    Button(combo.label) { model.sendCombo(i) }
                }
            } label: {
                toolbarIcon("command")
            }
            .tint(.white)
            .accessibilityLabel("Keys")
            .accessibilityHint("Send keys this device can't type, such as Ctrl+Alt+Del")
            .accessibilityIdentifier("keys")
            toolbarButton(model.soundOn ? "speaker.wave.2.fill" : "speaker.slash.fill",
                          label: "Sound") { model.toggleSound() }
                .opacity(model.soundOn ? 1 : 0.6)
                .accessibilityValue(model.soundOn ? "on" : "off")
                .accessibilityHint(model.soundOn ? "Plays the computer's sound: tap to mute"
                                                 : "The computer's sound is muted: tap to hear it")
                .accessibilityIdentifier("sound")
            toolbarButton("folder", label: "Files") { showingFiles = true }
                .accessibilityIdentifier("files")
            toolbarButton("doc.on.clipboard", label: "Paste to computer") { model.pasteToComputer() }
            toolbarButton("gearshape", label: "Input settings") { showingSettings = true }
                .accessibilityIdentifier("inputSettings")
            toolbarButton("xmark", label: "Disconnect") { model.disconnect() }
                .accessibilityIdentifier("disconnect")
        }
        .padding(.horizontal, 12)
        .padding(.top, 4)
    }

    private func gaveUpMessage(_ reason: String) -> some View {
        VStack(spacing: 14) {
            Text("Couldn't reconnect")
                .font(.headline)
            Text(reason)
                .font(.callout)
                .multilineTextAlignment(.center)
                .accessibilityIdentifier("gaveUpReason")
            HStack(spacing: 12) {
                Button("Close") { model.disconnect() }
                    .buttonStyle(.bordered)
                Button("Reconnect") { model.retry() }
                    .buttonStyle(.borderedProminent)
                    .accessibilityIdentifier("reconnect")
            }
        }
        .foregroundStyle(.white)
        .padding(20)
        .background(.black.opacity(0.75), in: RoundedRectangle(cornerRadius: 12))
        .padding(.horizontal, 24)
    }

    private func toolbarIcon(_ symbol: String) -> some View {
        Image(systemName: symbol)
            .font(.system(size: 15, weight: .semibold))
            .frame(width: 36, height: 36)
            .background(.ultraThinMaterial, in: Circle())
    }

    private func toolbarButton(_ symbol: String, label: String, action: @escaping () -> Void) -> some View {
        Button(action: action) { toolbarIcon(symbol) }
            .tint(.white)
            .accessibilityLabel(label)
    }
}

private struct RemoteScreen: UIViewRepresentable {
    let model: SessionModel
    @Binding var screen: RemoteScreenView?
    let settings: InputSettings
    let top: CGFloat
    let barTop: CGFloat?

    func makeUIView(context: Context) -> RemoteScreenView {
        let v = RemoteScreenView(model: model)
        v.apply(settings)
        DispatchQueue.main.async { screen = v }
        return v
    }

    func updateUIView(_ v: RemoteScreenView, context: Context) {
        v.apply(settings)
        v.reserve(top: top, bottomEdge: barTop)
    }
}

/// Left, middle and right buttons (held while your finger stays on them) and a strip that
/// scrolls when you drag along it, like RustDesk's.
private struct MouseButtonsBar: View {
    let input: PointerInput?
    @State private var lastDrag: CGFloat = 0

    var body: some View {
        HStack(spacing: 8) {
            HoldButton(title: "Left", id: "mouseLeft") { input?.bar(.left, pressed: $0) }
            HoldButton(title: "Middle", id: "mouseMiddle") { input?.bar(.middle, pressed: $0) }
            HoldButton(title: "Right", id: "mouseRight") { input?.bar(.right, pressed: $0) }
            Image(systemName: "arrow.up.and.down")
                .font(.system(size: 15, weight: .semibold))
                .foregroundStyle(.white)
                .frame(width: 44, height: 40)
                .background(Color.white.opacity(0.12), in: RoundedRectangle(cornerRadius: 10))
                .gesture(
                    DragGesture(minimumDistance: 2)
                        .onChanged { g in
                            let dy = g.translation.height - lastDrag
                            lastDrag = g.translation.height
                            input?.scroll(lines: CGPoint(x: 0, y: dy / 12))
                        }
                        .onEnded { _ in
                            lastDrag = 0
                            input?.scrollEnded()
                        }
                )
                .accessibilityElement()
                .accessibilityLabel("Scroll")
                .accessibilityHint("Drag up or down along it to scroll")
                .accessibilityIdentifier("mouseScroll")
                .accessibilityAdjustableAction { direction in
                    let lines: CGFloat = direction == .increment ? -3 : 3
                    input?.scroll(lines: CGPoint(x: 0, y: lines))
                    input?.scrollEnded()
                }
        }
        .padding(8)
        .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 14))
        .environment(\.colorScheme, .dark)
    }
}

/// A mouse button: down while touched, up when let go.
private struct HoldButton: View {
    let title: String
    let id: String
    let onChange: (Bool) -> Void
    @State private var down = false

    var body: some View {
        Text(title)
            .font(.callout.weight(.semibold))
            .foregroundStyle(.white)
            .frame(width: 62, height: 40)
            .background(Color.white.opacity(down ? 0.4 : 0.12), in: RoundedRectangle(cornerRadius: 10))
            .gesture(
                DragGesture(minimumDistance: 0)
                    .onChanged { _ in
                        if !down {
                            down = true
                            onChange(true)
                        }
                    }
                    .onEnded { _ in
                        down = false
                        onChange(false)
                    }
            )
            .accessibilityElement()
            .accessibilityLabel("\(title) button")
            .accessibilityAddTraits(.isButton)
            .accessibilityIdentifier(id)
            .accessibilityAction {
                onChange(true)
                onChange(false)
            }
    }
}
