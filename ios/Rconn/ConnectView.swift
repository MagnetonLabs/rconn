import SwiftUI

/// Server, computer ID and access password; recent computers below.
struct ConnectView: View {
    @AppStorage("server") private var server = ""
    @State private var fingerprint = ""
    @State private var computerID = ""
    @State private var password = ""
    @State private var recents = Recents.load()
    @State private var session: SessionModel?
    @State private var lastError: String?
    @FocusState private var focus: Field?

    private enum Field { case server, fingerprint, id, password }

    private var canConnect: Bool {
        !server.trimmingCharacters(in: .whitespaces).isEmpty
            && computerID.filter(\.isNumber).count >= 9
            && !password.isEmpty
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
                        .onSubmit(connect)
                        .accessibilityIdentifier("password")
                    Button(action: connect) {
                        Text("Connect").frame(maxWidth: .infinity).bold()
                    }
                    .disabled(!canConnect)
                    .accessibilityIdentifier("connect")
                } footer: {
                    if let lastError {
                        Text(lastError).foregroundStyle(.red)
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
                                focus = .password
                            } label: {
                                VStack(alignment: .leading) {
                                    Text(r.computerID).font(.body.monospacedDigit())
                                    Text(r.server).font(.caption).foregroundStyle(.secondary)
                                }
                            }
                        }
                        .onDelete { idx in
                            idx.map { recents[$0] }.forEach(Recents.remove)
                            recents = Recents.load()
                        }
                    }
                }
            }
            .navigationTitle("Rconn")
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

    private func connect() {
        guard canConnect else { return }
        let url = server.trimmingCharacters(in: .whitespaces)
        let pin = fingerprint.trimmingCharacters(in: .whitespaces)
        Keychain.setFingerprint(pin.isEmpty ? nil : pin, for: url)
        Recents.remember(server: url, computerID: computerID)
        recents = Recents.load()
        lastError = nil
        focus = nil
        session = SessionModel(server: url, fingerprint: pin.isEmpty ? nil : pin, computerID: computerID, password: password)
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
