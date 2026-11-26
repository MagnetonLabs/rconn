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
        var list = load().filter { !($0.server == server && $0.computerID == computerID) }
        list.insert(RecentComputer(server: server, computerID: computerID, lastUsed: Date()), at: 0)
        save(Array(list.prefix(limit)))
    }

    static func remove(_ c: RecentComputer) {
        save(load().filter { $0 != c })
    }

    private static func save(_ list: [RecentComputer]) {
        if let data = try? JSONEncoder().encode(list) {
            UserDefaults.standard.set(data, forKey: key)
        }
    }
}
