import AVFoundation

/// The app's audio session while a computer's sound plays. Rust plays the sound (cpal on
/// RemoteIO); this sets the category it plays under: `.playback`, so it is heard with the
/// ringer switch on silent, and not mixed with other apps' audio. Main thread only.
enum AudioSession {
    private static var wanted = false

    /// Before turning a session's sound on.
    static func activate() {
        wanted = true
        let s = AVAudioSession.sharedInstance()
        do {
            try s.setCategory(.playback, mode: .default, options: [])
            // Small hardware buffers: the host's sound should stay close to its picture.
            try? s.setPreferredIOBufferDuration(0.01)
            try s.setActive(true)
        } catch {
            NSLog("rconn: audio session: %@", error.localizedDescription)
        }
    }

    /// Sound is off or the session ended: let other apps' audio resume. A moment later,
    /// so the output Rust is closing has stopped (and not if sound came back on).
    static func deactivate() {
        wanted = false
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) {
            guard !wanted else { return }
            try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
        }
    }
}
