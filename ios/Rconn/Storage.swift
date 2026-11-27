import Foundation
import Security

/// The server's pinned key fingerprint lives in the Keychain (it decides whom the app
/// trusts, so it shouldn't sit in plain preferences or backups).
enum Keychain {
    private static let service = "app.rconn.ios.server-fingerprint"

    static func fingerprint(for server: String) -> String? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: server,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var item: CFTypeRef?
        guard SecItemCopyMatching(query as CFDictionary, &item) == errSecSuccess,
              let data = item as? Data
        else { return nil }
        return String(data: data, encoding: .utf8)
    }

    static func setFingerprint(_ value: String?, for server: String) {
        let base: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: server,
        ]
        SecItemDelete(base as CFDictionary)
        guard let value, !value.isEmpty else { return }
        var add = base
        add[kSecValueData as String] = Data(value.utf8)
        add[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        SecItemAdd(add as CFDictionary, nil)
    }
}

/// Access passwords the user chose to remember, one per server and computer ID; this
/// device only, never in backups.
enum SavedPasswords {
    private static let service = "app.rconn.ios.access-password"

    private static func base(_ server: String, _ computerID: String) -> [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: "\(server)|\(computerID.filter(\.isNumber))",
        ]
    }

    static func password(server: String, computerID: String) -> String? {
        var query = base(server, computerID)
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne
        var item: CFTypeRef?
        guard SecItemCopyMatching(query as CFDictionary, &item) == errSecSuccess,
              let data = item as? Data
        else { return nil }
        return String(data: data, encoding: .utf8)
    }

    @discardableResult
    static func set(_ password: String, server: String, computerID: String) -> Bool {
        forget(server: server, computerID: computerID)
        var add = base(server, computerID)
        add[kSecValueData as String] = Data(password.utf8)
        add[kSecAttrAccessible as String] = kSecAttrAccessibleWhenUnlockedThisDeviceOnly
        return SecItemAdd(add as CFDictionary, nil) == errSecSuccess
    }

    static func forget(server: String, computerID: String) {
        SecItemDelete(base(server, computerID) as CFDictionary)
    }
}

/// A computer connected to before (its password only in `SavedPasswords`, if remembered).
struct RecentComputer: Codable, Hashable, Identifiable {
    var id: String { "\(server)|\(computerID)" }
    var server: String
    var computerID: String
    var lastUsed: Date
    /// The computer's own name (sent once a session is allowed), or one typed here.
    var name: String? = nil
    /// `name` was typed here (Rename): the computer's own name doesn't replace it.
    var renamed: Bool? = nil
    /// Its access password is in `SavedPasswords`: a tap connects.
    var passwordSaved: Bool? = nil
}

enum Recents {
    private static let key = "recentComputers"
    private static let limit = 8

    /// Rconn's public server (`rconn_core::DEFAULT_SERVER`).
    static let publicServer = "wss://connect.rconn.app/v1/ws"

    /// Empty, or the example address the Server field used to show as its hint.
    static func isPlaceholder(_ server: String) -> Bool {
        let s = server.trimmingCharacters(in: .whitespaces)
        return s.isEmpty || s.contains("rconn.example.com")
    }

    /// Recent computers saved with the example address point at Rconn's server instead.
    static func replacePlaceholderServers() {
        let list = load()
        guard list.contains(where: { isPlaceholder($0.server) }) else { return }
        var seen = Set<String>()
        let fixed = list.map { r -> RecentComputer in
            var r = r
            if isPlaceholder(r.server) {
                if r.passwordSaved == true,
                   let pw = SavedPasswords.password(server: r.server, computerID: r.computerID) {
                    _ = SavedPasswords.set(pw, server: publicServer, computerID: r.computerID)
                    SavedPasswords.forget(server: r.server, computerID: r.computerID)
                }
                r.server = publicServer
            }
            return r
        }.filter { seen.insert($0.id).inserted }
        save(fixed)
    }

    static func load() -> [RecentComputer] {
        guard let data = UserDefaults.standard.data(forKey: key),
              let list = try? JSONDecoder().decode([RecentComputer].self, from: data)
        else { return [] }
        return list.sorted { $0.lastUsed > $1.lastUsed }
    }

    static func remember(server: String, computerID: String) {
        var list = load()
        var entry = RecentComputer(server: server, computerID: computerID, lastUsed: Date())
        if let old = list.first(where: { $0.id == entry.id }) {
            entry.name = old.name
            entry.renamed = old.renamed
            entry.passwordSaved = old.passwordSaved
        }
        list.removeAll { $0.id == entry.id }
        list.insert(entry, at: 0)
        for dropped in list.dropFirst(limit) where dropped.passwordSaved == true {
            SavedPasswords.forget(server: dropped.server, computerID: dropped.computerID)
        }
        save(Array(list.prefix(limit)))
    }

    static func remove(_ c: RecentComputer) {
        SavedPasswords.forget(server: c.server, computerID: c.computerID)
        save(load().filter { $0.id != c.id })
    }

    /// Note whether a computer's password is remembered.
    static func setPasswordSaved(server: String, computerID: String, _ saved: Bool) {
        save(load().map { r in
            guard r.server == server, r.computerID == computerID else { return r }
            var r = r
            r.passwordSaved = saved
            return r
        })
    }

    /// Name a computer here; empty goes back to the computer's own name (next session).
    static func rename(_ c: RecentComputer, to name: String) {
        let name = String(name.trimmingCharacters(in: .whitespacesAndNewlines).prefix(64))
        save(load().map { r in
            guard r.id == c.id else { return r }
            var r = r
            r.renamed = !name.isEmpty
            if !name.isEmpty { r.name = name }
            return r
        })
    }

    /// The computer told us its name: use it unless it was renamed here.
    static func learnName(server: String, computerID: String, name: String) {
        save(load().map { r in
            guard r.server == server, r.computerID == computerID, r.renamed != true else { return r }
            var r = r
            r.name = name
            return r
        })
    }

    private static func save(_ list: [RecentComputer]) {
        if let data = try? JSONEncoder().encode(list) {
            UserDefaults.standard.set(data, forKey: key)
        }
    }
}
