import XCTest
@testable import Rconn

final class ReconnectMachineTests: XCTestCase {
    private let t0 = Date(timeIntervalSince1970: 1_000_000)
    private let lost = "the connection to the computer was lost"

    private func connected() -> ReconnectMachine {
        var m = ReconnectMachine()
        m.pictureShown()
        return m
    }

    func testQuickAppSwitchKeepsTheSession() {
        var m = connected()
        m.enteredBackground()
        XCTAssertEqual(m.enteredForeground(now: t0), .none)
        XCTAssertEqual(m.phase, .live)
    }

    func testDropInBackgroundReconnectsOnReturn() {
        var m = connected()
        m.enteredBackground()
        // The grace period ended (the app closed it) or the network dropped it.
        XCTAssertEqual(m.sessionClosed(reason: "disconnected", now: t0), .none, "nothing while away")
        XCTAssertEqual(m.phase, .droppedInBackground(reason: "disconnected"))
        XCTAssertEqual(m.enteredForeground(now: t0 + 60), .reconnect(attempt: 1, delay: 0))
        m.pictureShown()
        XCTAssertEqual(m.phase, .live)
    }

    func testDropNoticedJustAfterReturningReconnects() {
        var m = connected()
        m.enteredBackground()
        XCTAssertEqual(m.enteredForeground(now: t0), .none)
        XCTAssertEqual(m.sessionClosed(reason: lost, now: t0 + 3), .reconnect(attempt: 1, delay: 0))
    }

    func testADropLongAfterReturningEndsTheSession() {
        var m = connected()
        m.enteredBackground()
        _ = m.enteredForeground(now: t0)
        XCTAssertEqual(m.sessionClosed(reason: lost, now: t0 + 120), .close(reason: lost))
    }

    func testGivesUpAfterThreeTriesThenRetryStartsAgain() {
        var m = connected()
        m.enteredBackground()
        _ = m.sessionClosed(reason: lost, now: t0)
        XCTAssertEqual(m.enteredForeground(now: t0 + 40), .reconnect(attempt: 1, delay: 0))
        XCTAssertEqual(m.sessionClosed(reason: "computer offline", now: t0 + 41), .reconnect(attempt: 2, delay: 2))
        XCTAssertEqual(m.sessionClosed(reason: "computer offline", now: t0 + 45), .reconnect(attempt: 3, delay: 5))
        XCTAssertEqual(m.sessionClosed(reason: "computer offline", now: t0 + 52), .giveUp(reason: "computer offline"))
        XCTAssertEqual(m.phase, .gaveUp(reason: "computer offline"))
        XCTAssertEqual(m.retry(), .reconnect(attempt: 1, delay: 0))
    }

    func testCloseAfterGivingUpLeaves() {
        var m = connected()
        m.enteredBackground()
        _ = m.sessionClosed(reason: lost, now: t0)
        _ = m.enteredForeground(now: t0 + 40)
        for _ in 0..<3 { _ = m.sessionClosed(reason: lost, now: t0 + 41) }
        XCTAssertEqual(m.userDisconnects(), .close(reason: "disconnected"))
    }

    func testUserDisconnectNeverReconnects() {
        var m = connected()
        XCTAssertEqual(m.userDisconnects(), .none)
        XCTAssertEqual(m.sessionClosed(reason: "disconnected", now: t0), .close(reason: "disconnected"))

        var away = connected()
        away.enteredBackground()
        _ = away.enteredForeground(now: t0)
        _ = away.userDisconnects()
        XCTAssertEqual(away.sessionClosed(reason: lost, now: t0 + 1), .close(reason: "disconnected"))
    }

    func testRefusalsAreFinal() {
        var m = connected()
        m.enteredBackground()
        _ = m.sessionClosed(reason: lost, now: t0)
        _ = m.enteredForeground(now: t0 + 40)
        let wrong = "wrong access password"
        XCTAssertEqual(m.sessionClosed(reason: wrong, now: t0 + 41), .close(reason: wrong))
        XCTAssertTrue(ReconnectMachine.isRefusal("the person at the computer declined"))
        XCTAssertFalse(ReconnectMachine.isRefusal(lost))
        XCTAssertFalse(ReconnectMachine.isRefusal("disconnected"))
    }

    func testNeverReconnectsASessionThatNeverShowedAPicture() {
        var m = ReconnectMachine()
        m.enteredBackground()
        XCTAssertEqual(m.sessionClosed(reason: lost, now: t0), .close(reason: lost))
    }

    func testPlainDropInTheForegroundEndsAsBefore() {
        var m = connected()
        XCTAssertEqual(m.sessionClosed(reason: "the computer ended the session", now: t0),
                       .close(reason: "the computer ended the session"))
    }
}
