import Foundation
import Security

enum NwcWakeInboxEvents {
    static let didChange = Notification.Name("RebelWalletNwcWakeInboxDidChange")
}

struct NwcWakeDebugEntry: Codable, Hashable, Identifiable {
    let id: String
    let timestamp: UInt64
    let source: String
    let message: String

    init(source: String, message: String, timestamp: UInt64 = UInt64(Date().timeIntervalSince1970)) {
        self.id = UUID().uuidString
        self.timestamp = timestamp
        self.source = source
        self.message = message
    }

    var timestampText: String {
        Date(timeIntervalSince1970: TimeInterval(timestamp)).formatted(date: .omitted, time: .standard)
    }
}

struct StoredNwcWakeRequest: Codable, Hashable {
    let relay: String
    let eventId: String
    let walletServicePubkey: String
    let eventJson: String?
    let receivedAt: UInt64

    init(
        relay: String,
        eventId: String,
        walletServicePubkey: String,
        eventJson: String? = nil,
        receivedAt: UInt64 = UInt64(Date().timeIntervalSince1970)
    ) {
        self.relay = relay
        self.eventId = eventId
        self.walletServicePubkey = walletServicePubkey
        self.eventJson = eventJson
        self.receivedAt = receivedAt
    }

    init?(userInfo: [AnyHashable: Any]) {
        guard
            (userInfo["protocol"] as? String) == "nwc_wake",
            let relay = userInfo["relay"] as? String,
            let eventId = userInfo["event_id"] as? String,
            let walletServicePubkey = userInfo["wallet_service_pubkey"] as? String
        else {
            return nil
        }

        self.init(
            relay: relay,
            eventId: eventId,
            walletServicePubkey: walletServicePubkey,
            eventJson: userInfo["nwc_event"] as? String
        )
    }

    static func parseFailureMessage(userInfo: [AnyHashable: Any]) -> String {
        let keys = userInfo.keys.map { String(describing: $0) }.sorted().joined(separator: ", ")
        guard (userInfo["protocol"] as? String) == "nwc_wake" else {
            return "Ignored push: protocol was \(String(describing: userInfo["protocol"])); keys: \(keys)"
        }
        if userInfo["relay"] as? String == nil {
            return "Invalid nwc_wake push: missing relay; keys: \(keys)"
        }
        if userInfo["event_id"] as? String == nil {
            return "Invalid nwc_wake push: missing event_id; keys: \(keys)"
        }
        if userInfo["wallet_service_pubkey"] as? String == nil {
            return "Invalid nwc_wake push: missing wallet_service_pubkey; keys: \(keys)"
        }
        return "Invalid nwc_wake push: fields were present but not parseable; keys: \(keys)"
    }

    var normalizedUserInfo: [AnyHashable: Any] {
        var info: [AnyHashable: Any] = [
            "protocol": "nwc_wake",
            "version": "v1",
            "relay": relay,
            "event_id": eventId,
            "wallet_service_pubkey": walletServicePubkey
        ]
        if let eventJson {
            info["nwc_event"] = eventJson
        }
        return info
    }
}

enum NwcWakeInbox {
    private static let queueKey = "nwcWakeQueue"
    private static let debugKey = "nwcWakeDebugLog"
    private static let snapshotKey = "nwcWakeSnapshot"
    private static let processedEventIdsKey = "nwcWakeProcessedEventIds"
    private static let maxDebugEntries = 30
    private static let maxProcessedEventIds = 100

    static func enqueue(_ request: StoredNwcWakeRequest) {
        guard let defaults = appGroupDefaults() else {
            NSLog("Could not open app group defaults for nwc_wake queue")
            return
        }
        guard !isProcessed(request.eventId, defaults: defaults) else {
            return
        }

        var requests = load(from: defaults)
        requests.removeAll { $0.eventId == request.eventId }
        requests.append(request)
        save(requests, to: defaults)
        NotificationCenter.default.post(name: NwcWakeInboxEvents.didChange, object: nil)
    }

    static func drain() -> [StoredNwcWakeRequest] {
        guard let defaults = appGroupDefaults() else {
            return []
        }

        let requests = load(from: defaults)
            .filter { !isProcessed($0.eventId, defaults: defaults) }
        let drainedIds = Set(requests.map(\.eventId))
        let remaining = load(from: defaults)
            .filter { !drainedIds.contains($0.eventId) && !isProcessed($0.eventId, defaults: defaults) }
        if remaining.isEmpty {
            defaults.removeObject(forKey: queueKey)
        } else {
            save(remaining, to: defaults)
        }
        return requests
    }

    static func appendDebug(source: String, message: String) {
        guard let defaults = appGroupDefaults() else {
            NSLog("Could not open app group defaults for nwc_wake debug log")
            return
        }

        var entries = debugEntries(from: defaults)
        entries.append(NwcWakeDebugEntry(source: source, message: message))
        if entries.count > maxDebugEntries {
            entries.removeFirst(entries.count - maxDebugEntries)
        }
        saveDebugEntries(entries, to: defaults)
        NotificationCenter.default.post(name: NwcWakeInboxEvents.didChange, object: nil)
    }

    static func debugEntries() -> [NwcWakeDebugEntry] {
        guard let defaults = appGroupDefaults() else {
            return []
        }
        return debugEntries(from: defaults)
    }

    static func clearDebugEntries() {
        guard let defaults = appGroupDefaults() else {
            return
        }
        defaults.removeObject(forKey: debugKey)
        NotificationCenter.default.post(name: NwcWakeInboxEvents.didChange, object: nil)
    }

    static func saveSnapshot(_ snapshotJson: String?) {
        guard let defaults = appGroupDefaults() else {
            NSLog("Could not open app group defaults for nwc_wake snapshot")
            return
        }

        if let snapshotJson, !snapshotJson.isEmpty {
            if !NwcWakeKeychainStore.set(snapshotJson, key: snapshotKey) {
                NSLog("Could not save NWC wake snapshot to Keychain")
            }
        } else {
            if !NwcWakeKeychainStore.delete(key: snapshotKey) {
                NSLog("Could not delete NWC wake snapshot from Keychain")
            }
        }
        defaults.removeObject(forKey: snapshotKey)
    }

    static func snapshot() -> String? {
        guard let defaults = appGroupDefaults() else {
            return nil
        }
        if let snapshot = NwcWakeKeychainStore.get(key: snapshotKey) {
            return snapshot
        }
        if let legacySnapshot = defaults.string(forKey: snapshotKey), !legacySnapshot.isEmpty {
            saveSnapshot(legacySnapshot)
            return legacySnapshot
        }
        return nil
    }

    static func markProcessed(eventId: String) {
        markProcessed(eventIds: [eventId])
    }

    static func markProcessed(eventIds: [String]) {
        guard let defaults = appGroupDefaults() else {
            return
        }

        let newIds = eventIds.reduce(into: [String]()) { uniqueIds, eventId in
            guard !eventId.isEmpty, !uniqueIds.contains(eventId) else {
                return
            }
            uniqueIds.append(eventId)
        }
        guard !newIds.isEmpty else {
            return
        }

        let newIdSet = Set(newIds)
        var ids = processedEventIds(from: defaults).filter { !newIdSet.contains($0) }
        ids.append(contentsOf: newIds)
        if ids.count > maxProcessedEventIds {
            ids.removeFirst(ids.count - maxProcessedEventIds)
        }
        defaults.set(ids, forKey: processedEventIdsKey)
        NotificationCenter.default.post(name: NwcWakeInboxEvents.didChange, object: nil)
    }

    static func unmarkProcessed(eventId: String) {
        guard let defaults = appGroupDefaults() else {
            return
        }

        var ids = processedEventIds(from: defaults)
        ids.removeAll { $0 == eventId }
        defaults.set(ids, forKey: processedEventIdsKey)
        NotificationCenter.default.post(name: NwcWakeInboxEvents.didChange, object: nil)
    }

    static func isProcessed(eventId: String) -> Bool {
        guard let defaults = appGroupDefaults() else {
            return false
        }
        return isProcessed(eventId, defaults: defaults)
    }

    private static func load(from defaults: UserDefaults) -> [StoredNwcWakeRequest] {
        guard let data = defaults.data(forKey: queueKey) else {
            return []
        }

        return (try? JSONDecoder().decode([StoredNwcWakeRequest].self, from: data)) ?? []
    }

    private static func save(_ requests: [StoredNwcWakeRequest], to defaults: UserDefaults) {
        guard let data = try? JSONEncoder().encode(requests) else {
            return
        }
        defaults.set(data, forKey: queueKey)
    }

    private static func debugEntries(from defaults: UserDefaults) -> [NwcWakeDebugEntry] {
        guard let data = defaults.data(forKey: debugKey) else {
            return []
        }

        return (try? JSONDecoder().decode([NwcWakeDebugEntry].self, from: data)) ?? []
    }

    private static func saveDebugEntries(_ entries: [NwcWakeDebugEntry], to defaults: UserDefaults) {
        guard let data = try? JSONEncoder().encode(entries) else {
            return
        }
        defaults.set(data, forKey: debugKey)
    }

    private static func processedEventIds(from defaults: UserDefaults) -> [String] {
        defaults.stringArray(forKey: processedEventIdsKey) ?? []
    }

    private static func isProcessed(_ eventId: String, defaults: UserDefaults) -> Bool {
        processedEventIds(from: defaults).contains(eventId)
    }

    private static func appGroupDefaults() -> UserDefaults? {
        guard
            let appGroupId = Bundle.main.object(forInfoDictionaryKey: "RebelWalletAppGroupIdentifier") as? String,
            !appGroupId.isEmpty
        else {
            return nil
        }
        return UserDefaults(suiteName: appGroupId)
    }
}

private enum NwcWakeKeychainStore {
    private static let service = "com.rebelwallet.app.nwc-wake"

    static func get(key: String) -> String? {
        var query = baseQuery(key: key)
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne

        var result: AnyObject?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        guard status == errSecSuccess, let data = result as? Data else {
            return nil
        }
        return String(data: data, encoding: .utf8)
    }

    static func set(_ value: String, key: String) -> Bool {
        let data = Data(value.utf8)
        var query = baseQuery(key: key)
        let update: [String: Any] = [kSecValueData as String: data]

        let updateStatus = SecItemUpdate(query as CFDictionary, update as CFDictionary)
        if updateStatus == errSecSuccess {
            return true
        }

        query[kSecValueData as String] = data
        query[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        return SecItemAdd(query as CFDictionary, nil) == errSecSuccess
    }

    static func delete(key: String) -> Bool {
        let status = SecItemDelete(baseQuery(key: key) as CFDictionary)
        return status == errSecSuccess || status == errSecItemNotFound
    }

    private static func baseQuery(key: String) -> [String: Any] {
        var query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: key
        ]
        if let accessGroup = keychainAccessGroup {
            query[kSecAttrAccessGroup as String] = accessGroup
        }
        return query
    }

    private static var keychainAccessGroup: String? {
        guard let value = Bundle.main.object(forInfoDictionaryKey: "RebelWalletKeychainAccessGroup") as? String else {
            return nil
        }
        let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty, !trimmed.hasPrefix("$(") else {
            return nil
        }
        return trimmed
    }
}
