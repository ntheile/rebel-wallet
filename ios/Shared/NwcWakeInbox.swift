import Foundation

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
    let receivedAt: UInt64

    init(relay: String, eventId: String, walletServicePubkey: String, receivedAt: UInt64 = UInt64(Date().timeIntervalSince1970)) {
        self.relay = relay
        self.eventId = eventId
        self.walletServicePubkey = walletServicePubkey
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

        self.init(relay: relay, eventId: eventId, walletServicePubkey: walletServicePubkey)
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
        [
            "protocol": "nwc_wake",
            "version": "v1",
            "relay": relay,
            "event_id": eventId,
            "wallet_service_pubkey": walletServicePubkey
        ]
    }
}

enum NwcWakeInbox {
    private static let queueKey = "nwcWakeQueue"
    private static let debugKey = "nwcWakeDebugLog"
    private static let maxDebugEntries = 30

    static func enqueue(_ request: StoredNwcWakeRequest) {
        guard let defaults = appGroupDefaults() else {
            NSLog("RebelWallet could not open app group defaults for nwc_wake queue")
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
        defaults.removeObject(forKey: queueKey)
        return requests
    }

    static func appendDebug(source: String, message: String) {
        guard let defaults = appGroupDefaults() else {
            NSLog("RebelWallet could not open app group defaults for nwc_wake debug log")
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
