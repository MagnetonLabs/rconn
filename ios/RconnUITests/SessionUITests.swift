import XCTest

/// Runs the app against `cargo run -p rconn-mobile --example test_host -- <password>`:
///
///   TEST_RUNNER_RCONN_TEST_SERVER=ws://127.0.0.1:8787/v1/ws TEST_RUNNER_RCONN_TEST_ID=<id> \
///   TEST_RUNNER_RCONN_TEST_PASSWORD=<password> TEST_RUNNER_RCONN_SHOTS=<folder> \
///   xcodebuild test -scheme Rconn -destination 'platform=iOS Simulator,name=…'
///
/// Opens the Keys menu and presses Ctrl+Alt+Del, switches the sound, browses the shared
/// folder, downloads a file and waits for the share sheet; checks touch input (a tap is a
/// left click and nothing else), the input settings and the mouse buttons bar, the
/// picture's place below the toolbar, and that leaving the app and coming back keeps (or
/// gets back) the session. With RCONN_SHOTS it saves a screenshot of each step there.
final class SessionUITests: XCTestCase {
    private var env: [String: String] { ProcessInfo.processInfo.environment }

    private func shot(_ name: String) {
        let png = XCUIScreen.main.screenshot().pngRepresentation
        let a = XCTAttachment(data: png, uniformTypeIdentifier: "public.png")
        a.name = name
        a.lifetime = .keepAlways
        add(a)
        if let dir = env["RCONN_SHOTS"] {
            try? FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
            try? png.write(to: URL(fileURLWithPath: dir).appendingPathComponent("\(name).png"))
        }
    }

    func testKeysSoundAndFiles() throws {
        let app = try launch()
        shot("1-session")

        app.buttons["keys"].tap()
        let cad = app.buttons["Ctrl+Alt+Del"]
        XCTAssertTrue(cad.waitForExistence(timeout: 5))
        for label in ["Task Manager (Ctrl+Shift+Esc)", "Windows key", "Alt+Tab", "Close window (Alt+F4)",
                      "Show desktop (Win+D)", "Run (Win+R)", "File Explorer (Win+E)", "Print Screen", "Insert"] {
            XCTAssertTrue(app.buttons[label].exists, label)
        }
        shot("2-keys-menu")
        cad.tap()

        let sound = app.buttons["sound"]
        let before = sound.value as? String
        sound.tap()
        XCTAssertNotEqual(sound.value as? String, before)
        shot("3-sound-switched")
        sound.tap()
        XCTAssertEqual(sound.value as? String, before)

        app.buttons["files"].tap()
        let notes = app.buttons["entry-notes.txt"]
        XCTAssertTrue(notes.waitForExistence(timeout: 20))
        shot("4-files-sheet")
        notes.tap()
        // The share sheet opens once the download is in and checked.
        let share = app.otherElements["ActivityListView"]
        let shareShown = share.waitForExistence(timeout: 20)
            || app.navigationBars["UIActivityContentView"].waitForExistence(timeout: 5)
        shot("5-downloaded-share")
        XCTAssertTrue(shareShown, "share sheet after the download")
    }

    func testTouchInputSettingsAndLayout() throws {
        let app = try launch()

        // The picture is centred in the space below the toolbar.
        let screen = app.windows.firstMatch.frame
        let toolbarBottom = app.buttons["disconnect"].frame.maxY
        let pic = pictureFrame(app)
        XCTAssertGreaterThanOrEqual(pic.minY, toolbarBottom, "the toolbar doesn't cover the picture")
        XCTAssertEqual(pic.midX, screen.midX, accuracy: 2)
        XCTAssertEqual(pic.midY, (toolbarBottom + 4 + screen.maxY) / 2, accuracy: 3)
        shot("touch-1-session-centred")

        // Touch mode (the default): one tap is one left click, where you tap. Never right.
        let a = CGPoint(x: pic.minX + pic.width * 0.25, y: pic.midY)
        XCTAssertEqual(eventsSent(app) { tap(app, at: a) }, ["left-down", "left-up"])
        XCTAssertEqual(pointer(app).x, a.x, accuracy: 3)
        XCTAssertEqual(pointer(app).y, a.y, accuracy: 3)

        // Settings: switch to Trackpad mode.
        app.buttons["inputSettings"].tap()
        XCTAssertTrue(app.buttons["inputDone"].waitForExistence(timeout: 5))
        shot("touch-2-input-settings")
        app.swipeUp()
        let tapHelp = app.descendants(matching: .any)["gesture-Tap"]
        XCTAssertTrue(tapHelp.waitForExistence(timeout: 3), "the gestures help")
        XCTAssertEqual(tapHelp.value as? String, "Left click where you tap")
        shot("touch-2b-gestures-help")
        app.swipeDown()
        app.buttons["Trackpad"].tap()
        XCTAssertTrue(app.buttons["Trackpad"].isSelected)
        app.swipeUp()
        XCTAssertEqual(app.descendants(matching: .any)["gesture-Tap"].value as? String, "Left click at the pointer",
                       "the gestures help follows the mode")
        app.buttons["inputDone"].tap()

        // Trackpad mode: a tap clicks where the pointer is; the pointer stays put.
        let b = CGPoint(x: pic.minX + pic.width * 0.75, y: pic.minY + pic.height * 0.8)
        XCTAssertEqual(eventsSent(app) { tap(app, at: b) }, ["left-down", "left-up"])
        XCTAssertEqual(pointer(app).x, a.x, accuracy: 3)
        XCTAssertEqual(pointer(app).y, a.y, accuracy: 3)

        // Mouse buttons bar: its buttons click; the picture moves out of its way.
        app.buttons["inputSettings"].tap()
        XCTAssertTrue(app.buttons["inputDone"].waitForExistence(timeout: 5))
        app.buttons["Touch"].tap()
        let toggle = app.switches["showMouseButtons"]
        if !toggle.isHittable { app.swipeUp() }
        toggle.switches.firstMatch.tap()
        app.buttons["inputDone"].tap()
        let right = app.buttons["mouseRight"]
        XCTAssertTrue(right.waitForExistence(timeout: 5))
        sleep(1)
        shot("touch-3-mouse-buttons")
        XCTAssertEqual(eventsSent(app) { right.tap() }, ["right-down", "right-up"])
        XCTAssertEqual(eventsSent(app) { app.buttons["mouseLeft"].tap() }, ["left-down", "left-up"])
        let barTop = app.buttons["mouseLeft"].frame.minY
        let withBar = pictureFrame(app)
        XCTAssertLessThanOrEqual(withBar.maxY, barTop, "the bar doesn't cover the picture")
        XCTAssertGreaterThanOrEqual(withBar.minY, toolbarBottom)

        // Landscape: still centred between the toolbar and the bar, nothing covered.
        XCUIDevice.shared.orientation = .landscapeLeft
        defer { XCUIDevice.shared.orientation = .portrait }
        sleep(2)
        let wide = app.windows.firstMatch.frame
        let top = app.buttons["disconnect"].frame.maxY
        let bottom = app.buttons["mouseLeft"].frame.minY
        let land = pictureFrame(app)
        XCTAssertGreaterThan(wide.width, wide.height)
        XCTAssertGreaterThanOrEqual(land.minY, top)
        XCTAssertLessThanOrEqual(land.maxY, bottom)
        XCTAssertEqual(land.midX, wide.midX, accuracy: 2)
        shot("touch-4-landscape")
    }

    func testLeavingTheAppKeepsOrRecoversTheSession() throws {
        // A short grace period stands in for iOS's ~25 s.
        let app = try launch(["RCONN_BACKGROUND_GRACE": "4"])
        XCTAssertEqual(pictures(app), 1)

        // A quick switch away and back: the same session goes on.
        XCUIDevice.shared.press(.home)
        sleep(2)
        app.activate()
        waitForPicture(app)
        sleep(3)
        XCTAssertEqual(pictures(app), 1, "a quick app switch keeps the session")
        XCTAssertTrue(app.buttons["keys"].exists)

        // Away longer than the grace period: the session ends, and coming back reconnects.
        XCUIDevice.shared.press(.home)
        sleep(10)
        app.activate()
        let again = expectation(for: NSPredicate { _, _ in self.pictures(app) == 2 }, evaluatedWith: nil)
        wait(for: [again], timeout: 40)
        waitForPicture(app)
        shot("background-reconnected")
        XCTAssertTrue(app.buttons["keys"].exists, "still in the session")
        let pic = pictureFrame(app)
        XCTAssertEqual(eventsSent(app) { tap(app, at: CGPoint(x: pic.midX, y: pic.midY)) }, ["left-down", "left-up"])
    }

    // MARK: Helpers

    /// Launch connected to the test host, from the default input settings; returns once
    /// the picture shows.
    private func launch(_ extra: [String: String] = [:]) throws -> XCUIApplication {
        guard let server = env["RCONN_TEST_SERVER"], let id = env["RCONN_TEST_ID"],
              let password = env["RCONN_TEST_PASSWORD"]
        else { throw XCTSkip("no test host (RCONN_TEST_SERVER, RCONN_TEST_ID, RCONN_TEST_PASSWORD)") }
        let app = XCUIApplication()
        app.launchEnvironment = [
            "RCONN_TEST_SERVER": server, "RCONN_TEST_ID": id, "RCONN_TEST_PASSWORD": password,
            "RCONN_RESET_INPUT": "1",
        ].merging(extra) { $1 }
        app.launch()
        XCTAssertTrue(app.buttons["keys"].waitForExistence(timeout: 30))
        waitForPicture(app)
        return app
    }

    /// Connected once the status line is gone (the picture shows).
    private func waitForPicture(_ app: XCUIApplication, timeout: TimeInterval = 30) {
        let gone = expectation(for: NSPredicate(format: "exists == false"), evaluatedWith: app.staticTexts["status"])
        wait(for: [gone], timeout: timeout)
    }

    /// The app's test probe: "pictures=1 picture=x,y,w,h input=left-down,left-up".
    private func probe(_ app: XCUIApplication) -> [String: String] {
        let label = app.descendants(matching: .any)["testProbe"].label
        var out: [String: String] = [:]
        for part in label.split(separator: " ") {
            let kv = part.split(separator: "=", maxSplits: 1)
            out[String(kv[0])] = kv.count > 1 ? String(kv[1]) : ""
        }
        return out
    }

    private func events(_ app: XCUIApplication) -> [String] {
        (probe(app)["input"] ?? "").split(separator: ",").map(String.init)
    }

    private func pictures(_ app: XCUIApplication) -> Int {
        Int(probe(app)["pictures"] ?? "") ?? -1
    }

    private func pictureFrame(_ app: XCUIApplication) -> CGRect {
        let n = (probe(app)["picture"] ?? "").split(separator: ",").compactMap { Double($0) }
        guard n.count == 4 else { return .zero }
        return CGRect(x: n[0], y: n[1], width: n[2], height: n[3])
    }

    /// The buttons and scrolls `act` sends (once they settle).
    private func eventsSent(_ app: XCUIApplication, by act: () -> Void) -> [String] {
        let before = events(app).count
        act()
        let deadline = Date().addingTimeInterval(5)
        while events(app).count < before + 2 && Date() < deadline { usleep(100_000) }
        sleep(1) // anything more would have arrived by now
        return Array(events(app).dropFirst(before))
    }

    private func tap(_ app: XCUIApplication, at p: CGPoint) {
        app.coordinate(withNormalizedOffset: .zero).withOffset(CGVector(dx: p.x, dy: p.y)).tap()
    }

    /// Where the remote pointer's tip is drawn.
    private func pointer(_ app: XCUIApplication) -> CGPoint {
        let f = app.descendants(matching: .any)["remotePointer"].frame
        return CGPoint(x: f.minX + 3, y: f.minY + 2)
    }
}
