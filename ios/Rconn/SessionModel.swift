import SwiftUI
import UIKit

/// One session with a computer: the Rust session plus what the UI shows about it.
/// Rust calls the `SessionListener` methods on its own threads.
final class SessionModel: ObservableObject, Identifiable, SessionListener, @unchecked Sendable {
    let id = UUID()
    let computerID: String
    let server: String
    /// Called on the main thread when the computer sends its name.
    var onHostName: ((String) -> Void)?
    /// Called on the main thread when the session ends, with the reason.
    var onClosedReason: ((String) -> Void)?
    let renderer = VideoRenderer()
    @Published var status = "connecting…"
    @Published var closedReason: String?
    /// The remote screen's size in pixels, once the first picture arrived.
    @Published var videoSize: CGSize = .zero
    /// Playing the computer's sound (remembered for the next session).
    @Published private(set) var soundOn: Bool
    private(set) var session: Session?
    /// The Keys menu (the desktop viewer's list, from Rust).
    let combos = keyCombos()
    private(set) lazy var files = FilesModel(session: session)

    init(server: String, fingerprint: String?, computerID: String, password: String) {
        self.computerID = computerID
        self.server = server
        let sound = UserDefaults.standard.object(forKey: "sound") as? Bool ?? true
        soundOn = sound
        if sound { AudioSession.activate() }
        let s = connectGuest(
            serverUrl: server, fingerprintPin: fingerprint, computerId: computerID,
            password: password, listener: self)
        session = s
        s.setSound(on: sound)
        renderer.onNeedKeyframe = { [weak s] in s?.requestKeyframe() }
        renderer.onVideoSize = { [weak self] size in
            DispatchQueue.main.async { self?.videoSize = size }
        }
    }

    func disconnect() {
        session?.disconnect()
    }

    /// Press entry `index` of the Keys menu on the computer.
    func sendCombo(_ index: Int) {
        session?.sendCombo(index: UInt32(index))
    }

    func toggleSound() {
        soundOn.toggle()
        UserDefaults.standard.set(soundOn, forKey: "sound")
        if soundOn { AudioSession.activate() }
        session?.setSound(on: soundOn)
        if !soundOn { AudioSession.deactivate() }
    }

    /// Put this device's clipboard text on the computer's clipboard.
    func pasteToComputer() {
        if let text = UIPasteboard.general.string {
            session?.sendClipboard(text: text)
        }
    }

    // MARK: SessionListener

    func onStatus(text: String) {
        DispatchQueue.main.async { self.status = text }
    }

    func onVideo(frame: VideoFrame) {
        renderer.enqueue(frame)
    }

    func onClipboard(text: String) {
        DispatchQueue.main.async { UIPasteboard.general.string = text }
    }

    func onHostName(name: String) {
        DispatchQueue.main.async {
            Recents.learnName(server: self.server, computerID: self.computerID, name: name)
            self.onHostName?(name)
        }
    }

    func onClosed(reason: String) {
        NSLog("rconn: session closed: %@ (%d frames shown)", reason, renderer.framesEnqueued)
        DispatchQueue.main.async {
            self.closedReason = reason
            self.session = nil
            if self.soundOn { AudioSession.deactivate() }
            self.onClosedReason?(reason)
        }
    }
}
