import SwiftUI

/// Server, computer ID and access password; recent computers below.
struct ConnectView: View {
    @AppStorage("server") private var server = ""
    @State private var fingerprint = ""
    @State private var computerID = ""
    @State private var password = ""
    /// "Remember this password" (the toggle starts as it was left).
    @AppStorage("rememberPasswords") private var rememberPassword = false
    @State private var recents = Recents.load()
    @State private var session: SessionModel?
    @State private var lastError: String?
    /// Recent computers the server says are online (by `RecentComputer.id`); nil: unknown.
    @State private var online: Set<String>?
    @State private var renaming: RecentComputer?
    @State private var newName = ""
    @FocusState private var focus: Field?

    private enum Field { case server, fingerprint, id, password }

    private var canConnect: Bool {
        !server.trimmingCharacters(in: .whitespaces).isEmpty
            && computerID.filter(\.isNumber).count >= 9
    }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    TextField("Computer ID", text: $computerID, prompt: Text("123 456 789"))
                        .keyboardType(.numberPad)
                        .font(.title3.monospacedDigit())
                        .focused($focus, equals: .id)
                        .accessibilityIdentifier("computerID")
                    SecureField("Access password", text: $password)
                        .textContentType(.password)
                        .focused($focus, equals: .password)
                        .submitLabel(.go)
                        .onSubmit { connect() }
                        .accessibilityIdentifier("password")
                    if !password.isEmpty {
                        Toggle("Remember this password", isOn: $rememberPassword)
                            .accessibilityIdentifier("rememberPassword")
                    }
                    Button { connect() } label: {
                        Text("Connect").frame(maxWidth: .infinity).bold()
                    }
                    .disabled(!canConnect)
                    .accessibilityIdentifier("connect")
                } footer: {
                    if let lastError {
                        Text(lastError).foregroundStyle(.red)
                    } else if password.isEmpty {
                        Text("No password: the person at the computer is asked to accept.")
                    }
                }

                Section {
                    TextField("Server", text: $server, prompt: Text("wss://rconn.example.com/v1/ws"))
                        .keyboardType(.URL)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .focused($focus, equals: .server)
                    TextField("Fingerprint (self-hosted)", text: $fingerprint, prompt: Text("sha256:… (optional)"))
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .font(.footnote.monospaced())
                        .focused($focus, equals: .fingerprint)
                } header: {
                    Text("Server")
                } footer: {
                    Text("The fingerprint pins a self-hosted server's key; leave it empty for a server with a public certificate.")
                }

                if !recents.isEmpty {
                    Section("Recent") {
                        ForEach(recents) { r in
                            Button {
                                server = r.server
                                computerID = r.computerID
                                fingerprint = Keychain.fingerprint(for: r.server) ?? ""
                                if r.passwordSaved == true,
                                   let saved = SavedPasswords.password(server: r.server, computerID: r.computerID) {
                                    connect(saved: saved)
                                } else {
                                    focus = .password
                                }
                            } label: {
                                HStack {
                                    VStack(alignment: .leading) {
                                        Text(r.name ?? "Unnamed computer")
                                        Text(r.computerID).font(.callout.monospacedDigit())
                                            .foregroundStyle(.secondary)
                                    }
                                    Spacer()
                                    if r.passwordSaved == true {
                                        Image(systemName: "key.fill")
                                            .font(.caption)
                                            .foregroundStyle(.secondary)
                                            .accessibilityLabel("Password remembered")
                                    }
                                    if let online {
                                        let up = online.contains(r.id)
                                        Label(up ? "online" : "offline", systemImage: "circle.fill")
                                            .font(.caption)
                                            .foregroundStyle(up ? .green : .secondary)
                                    }
                                }
                            }
                            .swipeActions {
                                Button("Remove", role: .destructive) {
                                    Recents.remove(r)
                                    recents = Recents.load()
                                }
                                if r.passwordSaved == true {
                                    Button("Forget password") { forgetPassword(r) }
                                        .tint(.orange)
                                }
                                Button("Rename") {
                                    newName = r.name ?? ""
                                    renaming = r
                                }
                                .tint(.blue)
                            }
                            .contextMenu {
                                Button("Rename", systemImage: "pencil") {
                                    newName = r.name ?? ""
                                    renaming = r
                                }
                                if r.passwordSaved == true {
                                    Button("Forget password", systemImage: "key.slash") { forgetPassword(r) }
                                }
                                Button("Remove", systemImage: "trash", role: .destructive) {
                                    Recents.remove(r)
                                    recents = Recents.load()
                                }
                            }
                        }
                    }
                }
            }
            .navigationTitle("Rconn")
            .alert("Rename", isPresented: Binding(
                get: { renaming != nil }, set: { if !$0 { renaming = nil } }
            )) {
                TextField("Name", text: $newName)
                Button("Save") {
                    if let r = renaming { Recents.rename(r, to: newName) }
                    recents = Recents.load()
                    renaming = nil
                }
                Button("Cancel", role: .cancel) { renaming = nil }
            } message: {
                Text("Empty: the computer's own name.")
            }
            .task {
                // Online or not, like the desktop app; every 15 s while this screen shows.
                while !Task.isCancelled {
                    await refreshOnline()
                    try? await Task.sleep(for: .seconds(15))
                }
            }
            .onAppear {
                fingerprint = Keychain.fingerprint(for: server) ?? ""
                autoConnectForTesting()
            }
            .onChange(of: server) { _, s in
                fingerprint = Keychain.fingerprint(for: s) ?? ""
            }
        }
        .fullScreenCover(item: $session) { model in
            SessionView(model: model) { reason in
                session = nil
                lastError = reason == "disconnected" ? nil : reason
            }
        }
    }

    /// Ask each recent computer's server which of them are online (no account needed).
    private func refreshOnline() async {
        let list = recents
        guard !list.isEmpty else { return }
        var up = Set<String>()
        var known = false
        for (server, group) in Dictionary(grouping: list, by: \.server) {
            let pin = Keychain.fingerprint(for: server)
            let ids = group.map(\.computerID)
            let answer = await Task.detached {
                onlineComputers(serverUrl: server, fingerprintPin: pin, computerIds: ids)
            }.value
            guard let answer else { continue }
            known = true
            for r in group where answer.contains(r.computerID) { up.insert(r.id) }
        }
        online = known ? up : nil
    }

    private func forgetPassword(_ r: RecentComputer) {
        SavedPasswords.forget(server: r.server, computerID: r.computerID)
        Recents.setPasswordSaved(server: r.server, computerID: r.computerID, false)
        recents = Recents.load()
    }

    /// Connect with the typed password, or with `saved` (a remembered one).
    private func connect(saved: String? = nil) {
        guard canConnect else { return }
        let url = server.trimmingCharacters(in: .whitespaces)
        let pin = fingerprint.trimmingCharacters(in: .whitespaces)
        let id = computerID
        let pw = saved ?? password
        Keychain.setFingerprint(pin.isEmpty ? nil : pin, for: url)
        Recents.remember(server: url, computerID: id)
        // Not ticked: nothing stays remembered for this computer.
        if saved == nil && (!rememberPassword || pw.isEmpty) {
            SavedPasswords.forget(server: url, computerID: id)
            Recents.setPasswordSaved(server: url, computerID: id, false)
        }
        recents = Recents.load()
        lastError = nil
        focus = nil
        let model = SessionModel(server: url, fingerprint: pin.isEmpty ? nil : pin, computerID: id, password: pw)
        let keep = saved == nil && rememberPassword && !pw.isEmpty
        // The computer sends its name only once it has let us in: then the password worked.
        model.onHostName = { _ in
            if keep {
                let ok = SavedPasswords.set(pw, server: url, computerID: id)
                Recents.setPasswordSaved(server: url, computerID: id, ok)
            }
            recents = Recents.load()
        }
        if saved != nil {
            model.onClosedReason = { reason in
                guard reason.contains("wrong access password") else { return }
                SavedPasswords.forget(server: url, computerID: id)
                Recents.setPasswordSaved(server: url, computerID: id, false)
                recents = Recents.load()
                lastError = "The remembered password no longer works: type the current one."
                focus = .password
            }
        }
        session = model
        password = ""
    }

    /// Simulator test aid: `RCONN_TEST_SERVER`, `RCONN_TEST_ID` and `RCONN_TEST_PASSWORD`
    /// in the launch environment connect straight away.
    private func autoConnectForTesting() {
        let env = ProcessInfo.processInfo.environment
        guard session == nil, let s = env["RCONN_TEST_SERVER"], let id = env["RCONN_TEST_ID"],
              let pw = env["RCONN_TEST_PASSWORD"]
        else { return }
        server = s
        computerID = id
        password = pw
        connect()
    }
}
