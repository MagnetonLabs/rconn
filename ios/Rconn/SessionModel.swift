import SwiftUI
import UIKit

/// One session with a computer: the Rust session plus what the UI shows about it.
///
/// Going to the background keeps the session for the time iOS allows (about 25 s) with the
/// picture paused; a quick app switch doesn't drop it. If it drops anyway (longer away, or
/// the network noticed), coming back reconnects to the same computer with the same
/// password, which stays in memory only (`ReconnectMachine` decides when).
final class SessionModel: ObservableObject, Identifiable, @unchecked Sendable {
    let id = UUID()
    let computerID: String
    let server: String
    private let fingerprint: String?
    /// Held in memory for reconnecting; never written anywhere by this model.
    private let password: String
    /// Called on the main thread when the computer sends its name.
    var onHostName: ((String) -> Void)?
    /// Called on the main thread when the session ends for good, with the reason.
    var onClosedReason: ((String) -> Void)?
    let renderer = VideoRenderer()
    @Published var status = "connecting…"
    @Published var closedReason: String?
    /// The remote screen's size in pixels, once the first picture arrived.
    @Published var videoSize: CGSize = .zero
    /// Playing the computer's sound (remembered for the next session).
    @Published private(set) var soundOn: Bool
    /// "Reconnecting… (2 of 3)" while trying to get a dropped session back.
    @Published private(set) var reconnecting: String?
    /// Why reconnecting gave up (the Reconnect button shows with it).
    @Published private(set) var gaveUp: String?
    /// Counts sessions: a reconnect replaces the session (and adds one).
    @Published private(set) var generation = 0
    private(set) var session: Session?
    /// The Keys menu (the desktop viewer's list, from Rust).
    let combos = keyCombos()
    private(set) lazy var files = FilesModel { [weak self] in self?.session }

    private var machine = ReconnectMachine()
    private var backgroundTask = UIBackgroundTaskIdentifier.invalid
    private var graceWork: DispatchWorkItem?
    private var observers: [NSObjectProtocol] = []
    /// The generation whose callbacks count (Rust calls from its own threads).
    private let current = Locked(0)

    /// How long to keep the session in the background: what iOS allows, less a margin to
    /// close it cleanly. `RCONN_BACKGROUND_GRACE` (seconds) shortens it for tests.
    private static let backgroundGrace: TimeInterval = {
        if let s = ProcessInfo.processInfo.environment["RCONN_BACKGROUND_GRACE"], let t = TimeInterval(s) { return t }
        return 25
    }()

    init(server: String, fingerprint: String?, computerID: String, password: String) {
        self.computerID = computerID
        self.server = server
        self.fingerprint = fingerprint
        self.password = password
        let sound = UserDefaults.standard.object(forKey: "sound") as? Bool ?? true
        soundOn = sound
        if sound { AudioSession.activate() }
        renderer.onVideoSize = { [weak self] size in
            DispatchQueue.main.async { self?.videoSize = size }
        }
        let nc = NotificationCenter.default
        observers = [
            nc.addObserver(forName: UIApplication.didEnterBackgroundNotification, object: nil, queue: .main) {
                [weak self] _ in self?.enteredBackground()
            },
            nc.addObserver(forName: UIApplication.willEnterForegroundNotification, object: nil, queue: .main) {
                [weak self] _ in self?.enteredForeground()
            },
        ]
        start()
    }

    deinit {
        observers.forEach(NotificationCenter.default.removeObserver)
        endBackgroundTask()
    }

    /// A new Rust session with the same computer, password and sound setting.
    private func start() {
        generation += 1
        current.set(generation)
        let s = connectGuest(
            serverUrl: server, fingerprintPin: fingerprint, computerId: computerID,
            password: password, listener: Listener(model: self, generation: generation))
        session = s
        s.setSound(on: soundOn)
        renderer.onNeedKeyframe = { [weak s] in s?.requestKeyframe() }
    }

    func disconnect() {
        let action = machine.userDisconnects()
        if let session {
            // Its end arrives in `closed`, which then closes.
            session.disconnect()
            handle(action)
        } else {
            // Between attempts, or gave up: nothing running to report an end.
            handle(.close(reason: "disconnected"))
        }
    }

    /// The Reconnect button after giving up.
    func retry() {
        handle(machine.retry())
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

    // MARK: Background

    private func enteredBackground() {
        machine.enteredBackground()
        renderer.pause() // nothing to show; saves the decoder's work
        guard session != nil else { return }
        endBackgroundTask()
        backgroundTask = UIApplication.shared.beginBackgroundTask(withName: "rconn session") { [weak self] in
            // iOS's time is up before ours (it shouldn't be): stop now.
            self?.closeForBackground()
            self?.endBackgroundTask()
        }
        let remaining = UIApplication.shared.backgroundTimeRemaining
        let grace = min(Self.backgroundGrace, max(remaining - 3, 0))
        let work = DispatchWorkItem { [weak self] in
            self?.closeForBackground()
            // A moment for the goodbye to go out before iOS suspends the app.
            DispatchQueue.main.asyncAfter(deadline: .now() + 1) { self?.endBackgroundTask() }
        }
        graceWork = work
        DispatchQueue.main.asyncAfter(deadline: .now() + grace, execute: work)
    }

    /// The grace period is over: end the session cleanly (the computer sees us leave at
    /// once, rather than after its own timeout); coming back reconnects.
    private func closeForBackground() {
        graceWork?.cancel()
        graceWork = nil
        guard machine.phase == .background, let session else { return }
        NSLog("rconn: background grace over; closing the session (reconnects on return)")
        session.disconnect()
    }

    private func enteredForeground() {
        graceWork?.cancel()
        graceWork = nil
        endBackgroundTask()
        renderer.resume()
        session?.requestKeyframe()
        handle(machine.enteredForeground(now: Date()))
    }

    private func endBackgroundTask() {
        guard backgroundTask != .invalid else { return }
        UIApplication.shared.endBackgroundTask(backgroundTask)
        backgroundTask = .invalid
    }

    // MARK: Reconnecting

    private func handle(_ action: ReconnectMachine.Action) {
        switch action {
        case .none:
            break
        case let .reconnect(attempt, delay):
            gaveUp = nil
            reconnecting = "Reconnecting… (\(attempt) of \(ReconnectMachine.maxAttempts))"
            status = "reconnecting…"
            NSLog("rconn: reconnecting, attempt %d", attempt)
            DispatchQueue.main.asyncAfter(deadline: .now() + delay) { [weak self] in
                guard let self, self.machine.phase == .reconnecting(attempt: attempt) else { return }
                self.start()
            }
        case let .giveUp(reason):
            reconnecting = nil
            status = ""
            gaveUp = reason
        case let .close(reason):
            reconnecting = nil
            gaveUp = nil
            closedReason = reason
            if soundOn { AudioSession.deactivate() }
            onClosedReason?(reason)
        }
    }

    // MARK: From Rust (main thread, current generation only)

    fileprivate func statusChanged(_ text: String) {
        if text.isEmpty {
            // The picture shows.
            machine.pictureShown()
            reconnecting = nil
            TestProbe.shared.pictureShown()
        } else if reconnecting != nil {
            status = "reconnecting: \(text)"
            return
        }
        status = text
    }

    fileprivate func closed(_ reason: String) {
        NSLog("rconn: session closed: %@ (%d frames shown)", reason, renderer.framesEnqueued)
        session = nil
        handle(machine.sessionClosed(reason: reason, now: Date()))
    }

    fileprivate func hostName(_ name: String) {
        Recents.learnName(server: server, computerID: computerID, name: name)
        onHostName?(name)
    }

    fileprivate func isCurrent(_ generation: Int) -> Bool {
        current.get() == generation
    }

    /// Rust calls these on its own threads; anything from a replaced session is dropped.
    private final class Listener: SessionListener, @unchecked Sendable {
        private weak var model: SessionModel?
        private let generation: Int

        init(model: SessionModel, generation: Int) {
            self.model = model
            self.generation = generation
        }

        private func onMain(_ f: @escaping (SessionModel) -> Void) {
            DispatchQueue.main.async { [weak model, generation] in
                guard let model, model.isCurrent(generation) else { return }
                f(model)
            }
        }

        func onStatus(text: String) { onMain { $0.statusChanged(text) } }

        func onVideo(frame: VideoFrame) {
            guard let model, model.isCurrent(generation) else { return }
            model.renderer.enqueue(frame)
        }

        func onClipboard(text: String) { onMain { _ in UIPasteboard.general.string = text } }

        func onHostName(name: String) { onMain { $0.hostName(name) } }

        func onClosed(reason: String) { onMain { $0.closed(reason) } }
    }
}

/// A value shared between threads.
final class Locked<T>: @unchecked Sendable {
    private var value: T
    private let lock = NSLock()

    init(_ value: T) { self.value = value }

    func get() -> T {
        lock.lock()
        defer { lock.unlock() }
        return value
    }

    func set(_ v: T) {
        lock.lock()
        value = v
        lock.unlock()
    }
}
