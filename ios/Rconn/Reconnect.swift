import Foundation

/// When a session that dropped because the app went to the background comes back by
/// itself. Pure state (time passed in), so it is unit-tested as it is.
///
/// - In the background the session is kept for the grace period iOS allows; when that
///   ends (or the connection drops meanwhile) the session is over, quietly.
/// - Back in the foreground, a session that dropped is reconnected: same computer, same
///   password (held in memory only), up to `maxAttempts` tries, then a message with a
///   Reconnect button.
/// - A drop shortly after coming back also counts (the network may take a moment to
///   notice what happened while the app was suspended).
/// - Never after the user disconnected, never for a refusal (wrong password, declined),
///   and never for a session that hadn't shown a picture yet.
struct ReconnectMachine: Equatable {
    enum Phase: Equatable {
        /// Connecting for the first time, or connected.
        case live
        /// In the background, the session (maybe) still up.
        case background
        /// The session ended while in the background; reconnect on return.
        case droppedInBackground(reason: String)
        /// Trying again (`attempt` of `maxAttempts`).
        case reconnecting(attempt: Int)
        /// Gave up: the message and a Reconnect button show.
        case gaveUp(reason: String)
        /// Over for good: back to the connect screen with this reason.
        case closed(reason: String)
    }

    /// What the session model should do now.
    enum Action: Equatable {
        case none
        /// Start a new session after `delay` seconds.
        case reconnect(attempt: Int, delay: TimeInterval)
        /// Show the give-up message (with Reconnect and Close).
        case giveUp(reason: String)
        /// Leave the session screen.
        case close(reason: String)
    }

    static let maxAttempts = 3
    /// Before each attempt: at once, then a little longer each time.
    static let delays: [TimeInterval] = [0, 2, 5]
    /// A drop this soon after returning counts as caused by the background.
    static let afterForeground: TimeInterval = 10

    private(set) var phase = Phase.live
    /// A session showed a picture (only then is it worth coming back to).
    private(set) var everConnected = false
    private var userClosing = false
    private var foregroundAt: Date?

    /// The picture showed: the (re)connection worked.
    mutating func pictureShown() {
        everConnected = true
        if case .reconnecting = phase { phase = .live }
    }

    mutating func enteredBackground() {
        if phase == .live { phase = .background }
    }

    mutating func enteredForeground(now: Date) -> Action {
        foregroundAt = now
        switch phase {
        case .background:
            phase = .live
            return .none
        case .droppedInBackground:
            return attempt(1)
        default:
            return .none
        }
    }

    /// The user tapped Disconnect (or Close on the give-up message).
    mutating func userDisconnects() -> Action {
        userClosing = true
        switch phase {
        case .gaveUp, .droppedInBackground:
            // No session is running to report its end: close now.
            phase = .closed(reason: "disconnected")
            return .close(reason: "disconnected")
        case .reconnecting:
            // The attempt's session reports its end; `sessionClosed` closes then.
            return .none
        default:
            return .none
        }
    }

    /// The user tapped Reconnect on the give-up message.
    mutating func retry() -> Action {
        guard case .gaveUp = phase else { return .none }
        return attempt(1)
    }

    /// The running session ended with `reason`.
    mutating func sessionClosed(reason: String, now: Date) -> Action {
        if userClosing || Self.isRefusal(reason) {
            phase = .closed(reason: userClosing ? "disconnected" : reason)
            return .close(reason: userClosing ? "disconnected" : reason)
        }
        switch phase {
        case .background:
            guard everConnected else { return close(reason) }
            phase = .droppedInBackground(reason: reason)
            return .none
        case .reconnecting(let n):
            if n < Self.maxAttempts { return attempt(n + 1) }
            phase = .gaveUp(reason: reason)
            return .giveUp(reason: reason)
        case .live:
            if everConnected, let back = foregroundAt, now.timeIntervalSince(back) <= Self.afterForeground {
                return attempt(1)
            }
            return close(reason)
        case .droppedInBackground, .gaveUp, .closed:
            return .none
        }
    }

    private mutating func attempt(_ n: Int) -> Action {
        phase = .reconnecting(attempt: n)
        return .reconnect(attempt: n, delay: Self.delays[min(n - 1, Self.delays.count - 1)])
    }

    private mutating func close(_ reason: String) -> Action {
        phase = .closed(reason: reason)
        return .close(reason: reason)
    }

    /// The computer said no: trying again would only say no again (or ask the person at
    /// it again and again).
    static func isRefusal(_ reason: String) -> Bool {
        let r = reason.lowercased()
        return ["password", "declined", "refus", "not allowed", "denied"]
            .contains { r.contains($0) }
    }
}
