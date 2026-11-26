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

/// A computer connected to before (never its password).
struct RecentComputer: Codable, Hashable, Identifiable {
    var id: String { "\(server)|\(computerID)" }
    var server: String
    var computerID: String
    var lastUsed: Date
    /// The computer's own name (sent once a session is allowed), or one typed here.
    var name: String? = nil
    /// `name` was typed here (Rename): the computer's own name doesn't replace it.
    var renamed: Bool? = nil
}

enum Recents {
    private static let key = "recentComputers"
    private static let limit = 8

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
        }
        list.removeAll { $0.id == entry.id }
        list.insert(entry, at: 0)
        save(Array(list.prefix(limit)))
    }

    static func remove(_ c: RecentComputer) {
        save(load().filter { $0.id != c.id })
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
