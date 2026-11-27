import SwiftUI

@main
struct RconnApp: App {
    init() {
        TestProbe.resetInputSettingsIfAsked()
    }

    var body: some Scene {
        WindowGroup {
            ConnectView()
        }
    }
}
