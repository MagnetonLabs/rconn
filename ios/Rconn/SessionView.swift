import SwiftUI

/// Full-screen session: the remote screen, a status line until the picture shows, and a
/// small toolbar (keyboard, paste, disconnect).
struct SessionView: View {
    @ObservedObject var model: SessionModel
    let onClose: (String) -> Void
    @State private var screen: RemoteScreenView?

    var body: some View {
        ZStack(alignment: .top) {
            RemoteScreen(model: model, screen: $screen)
                .ignoresSafeArea()

            if !model.status.isEmpty {
                VStack(spacing: 12) {
                    ProgressView().tint(.white)
                    Text(model.status)
                        .foregroundStyle(.white)
                        .multilineTextAlignment(.center)
                        .accessibilityIdentifier("status")
                }
                .padding(20)
                .background(.black.opacity(0.6), in: RoundedRectangle(cornerRadius: 12))
                .frame(maxHeight: .infinity)
            }

            HStack(spacing: 10) {
                Text(model.computerID)
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(.white.opacity(0.8))
                Spacer()
                toolbarButton("keyboard", label: "Keyboard") { screen?.showKeyboard() }
                toolbarButton("doc.on.clipboard", label: "Paste to computer") { model.pasteToComputer() }
                toolbarButton("xmark", label: "Disconnect") { model.disconnect() }
                    .accessibilityIdentifier("disconnect")
            }
            .padding(.horizontal, 12)
            .padding(.top, 4)
        }
        .background(Color.black)
        .ignoresSafeArea(.keyboard) // the soft keyboard covers the picture; no resize
        .statusBarHidden()
        .persistentSystemOverlays(.hidden)
        .onChange(of: model.videoSize) { _, _ in screen?.videoSizeChanged() }
        .onChange(of: model.closedReason) { _, reason in
            if let reason { onClose(reason) }
        }
    }

    private func toolbarButton(_ symbol: String, label: String, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Image(systemName: symbol)
                .font(.system(size: 15, weight: .semibold))
                .frame(width: 36, height: 36)
                .background(.ultraThinMaterial, in: Circle())
        }
        .tint(.white)
        .accessibilityLabel(label)
    }
}

private struct RemoteScreen: UIViewRepresentable {
    let model: SessionModel
    @Binding var screen: RemoteScreenView?

    func makeUIView(context: Context) -> RemoteScreenView {
        let v = RemoteScreenView(model: model)
        DispatchQueue.main.async { screen = v }
        return v
    }

    func updateUIView(_ uiView: RemoteScreenView, context: Context) {}
}
