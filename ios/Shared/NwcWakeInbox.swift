import Foundation
import NwcMobileApple
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

extension NwcQueuedWakeRequest {
    init?(validatedUserInfo userInfo: [AnyHashable: Any]) {
        let receivedAt = UInt64(Date().timeIntervalSince1970)
        guard
            let data = try? JSONSerialization.data(withJSONObject: userInfo),
            let payloadJson = String(data: data, encoding: .utf8),
            let request = parseNwcWakePayloadJson(
                payloadJson: payloadJson,
                receivedAtSeconds: receivedAt
            )
        else {
            return nil
        }
        self.init(
            payload: NwcWakePayload(
                relayURL: request.relayUrl,
                eventIDHex: request.eventIdHex,
                walletServicePublicKeyHex: request.walletServicePublicKeyHex,
                embeddedEventJSON: request.embeddedEventJson
            ),
            receivedAtSeconds: request.receivedAtSeconds
        )
    }

    static let parseFailureMessage = "Ignored malformed or unrelated wake notification"
}

extension NwcWakePayload {
    var normalizedUserInfo: [AnyHashable: Any] {
        var info: [AnyHashable: Any] = [
            NwcWakePayloadKey.relayURL: relayURL,
            NwcWakePayloadKey.eventID: eventIDHex,
            NwcWakePayloadKey.walletServicePublicKey: walletServicePublicKeyHex,
        ]
        if let embeddedEventJSON {
            info[NwcWakePayloadKey.embeddedEvent] = embeddedEventJSON
        }
        return info
    }
}

enum NwcWakeInbox {
    private static let queueKey = "nwcWakeQueue"
    private static let debugKey = "nwcWakeDebugLog"
    private static let snapshotKey = "nwcWakeSnapshot"
    private static let legacyProcessedEventIdsKey = "nwcWakeProcessedEventIds"
    private static let maxDebugEntries = 30

    static func enqueue(_ request: NwcQueuedWakeRequest) {
        do {
            let inbox = try appGroupInbox()
            try migrateLegacyQueueIfNeeded(to: inbox)
            try inbox.enqueue(request)
        } catch {
            NSLog("Could not persist nwc_wake request: %@", String(describing: error))
            return
        }
        NotificationCenter.default.post(name: NwcWakeInboxEvents.didChange, object: nil)
    }

    static func pendingRequests() -> [NwcQueuedWakeRequest] {
        do {
            let inbox = try appGroupInbox()
            try migrateLegacyQueueIfNeeded(to: inbox)
            return try inbox.pendingRequests()
        } catch {
            NSLog("Could not read nwc_wake requests: %@", String(describing: error))
            return []
        }
    }

    static func remove(eventIds: Set<String>) {
        guard !eventIds.isEmpty else { return }
        do {
            let changed = try appGroupInbox().remove(eventIDs: eventIds)
            if changed {
                NotificationCenter.default.post(name: NwcWakeInboxEvents.didChange, object: nil)
            }
        } catch {
            NSLog("Could not acknowledge nwc_wake requests: %@", String(describing: error))
        }
    }

    static func appendDebug(source: String, message: String) {
#if DEBUG
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
#endif
    }

    static func debugEntries() -> [NwcWakeDebugEntry] {
#if DEBUG
        guard let defaults = appGroupDefaults() else {
            return []
        }
        return debugEntries(from: defaults)
#else
        return []
#endif
    }

    static func clearDebugEntries() {
#if DEBUG
        guard let defaults = appGroupDefaults() else {
            return
        }
        defaults.removeObject(forKey: debugKey)
        NotificationCenter.default.post(name: NwcWakeInboxEvents.didChange, object: nil)
#endif
    }

    static func removeLegacySnapshot() {
        if let defaults = appGroupDefaults() {
            defaults.removeObject(forKey: snapshotKey)
            defaults.removeObject(forKey: legacyProcessedEventIdsKey)
        }
        NwcWakeKeychainStore.delete(key: snapshotKey)
    }

    static func extensionDataDirectoryPath() -> String? {
        try? appGroupInbox().dataDirectoryURL().path
    }

    private static func appGroupInbox() throws -> NwcAppGroupWakeInbox {
        guard
            let appGroupId = appGroupIdentifier(),
            let inbox = NwcAppGroupWakeInbox(appGroupIdentifier: appGroupId)
        else {
            throw CocoaError(.fileNoSuchFile)
        }
        return inbox
    }

    private static func migrateLegacyQueueIfNeeded(to inbox: NwcAppGroupWakeInbox) throws {
        if let defaults = appGroupDefaults() {
            try inbox.migrateLegacyFlatQueue(from: defaults, key: queueKey)
        }
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

    private static func appGroupDefaults() -> UserDefaults? {
        guard let appGroupId = appGroupIdentifier() else { return nil }
        return UserDefaults(suiteName: appGroupId)
    }

    private static func appGroupIdentifier() -> String? {
        guard
            let appGroupId = Bundle.main.object(
                forInfoDictionaryKey: "RebelWalletAppGroupIdentifier"
            ) as? String,
            !appGroupId.isEmpty
        else {
            return nil
        }
        return appGroupId
    }
}

private enum NwcWakeKeychainStore {
    private static let service = "com.rebelwallet.app.nwc-wake"

    static func delete(key: String) {
        SecItemDelete(baseQuery(key: key) as CFDictionary)
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
