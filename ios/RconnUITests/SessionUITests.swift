import XCTest

/// Runs the app against `cargo run -p rconn-mobile --example test_host -- <password>`:
///
///   TEST_RUNNER_RCONN_TEST_SERVER=ws://127.0.0.1:8787/v1/ws TEST_RUNNER_RCONN_TEST_ID=<id> \
///   TEST_RUNNER_RCONN_TEST_PASSWORD=<password> TEST_RUNNER_RCONN_SHOTS=<folder> \
///   xcodebuild test -scheme Rconn -destination 'platform=iOS Simulator,name=…'
///
/// Opens the Keys menu and presses Ctrl+Alt+Del, switches the sound, browses the shared
/// folder, downloads a file and waits for the share sheet. With RCONN_SHOTS it saves a
/// screenshot of each step there.
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
        guard let server = env["RCONN_TEST_SERVER"], let id = env["RCONN_TEST_ID"],
              let password = env["RCONN_TEST_PASSWORD"]
        else { throw XCTSkip("no test host (RCONN_TEST_SERVER, RCONN_TEST_ID, RCONN_TEST_PASSWORD)") }
        let app = XCUIApplication()
        app.launchEnvironment = [
            "RCONN_TEST_SERVER": server, "RCONN_TEST_ID": id, "RCONN_TEST_PASSWORD": password,
        ]
        app.launch()

        // Connected once the status line is gone (the picture shows).
        let keys = app.buttons["keys"]
        XCTAssertTrue(keys.waitForExistence(timeout: 30))
        let status = app.staticTexts["status"]
        let gone = expectation(for: NSPredicate(format: "exists == false"), evaluatedWith: status)
        wait(for: [gone], timeout: 30)
        shot("1-session")

        keys.tap()
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
}
