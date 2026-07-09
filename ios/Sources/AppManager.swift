import Foundation
import Observation
import Security
import UIKit
import UserNotifications

@MainActor
@Observable
final class AppManager: AppReconciler {
    let rust: FfiApp
    var state: AppState
    var nwcWakeDebugEntries: [NwcWakeDebugEntry]
    var pendingNwaWalletRequest: NwaWalletCreatedRequest?
    private var lastRevApplied: UInt64
    private var lastNwcWakeStatusLogged: String
    private var lastNwcWakeSnapshot: String?
    private var lastReceiveNotificationKey: String?
    private var receiveBackgroundTask: UIBackgroundTaskIdentifier = .invalid
    private var notificationObservers: [NSObjectProtocol] = []
    private let nwcWakeRegistration = NwcWakeRegistrationService()

    init(storagePaths: AppStoragePaths) {
        let dataDir = storagePaths.dataDir
        let cacheDir = storagePaths.cacheDir
        let rust = FfiApp(dataDir: dataDir, cacheDir: cacheDir, secretStore: KeychainSecretStore())
        self.rust = rust

        let initial = rust.state()
        self.state = initial
        self.nwcWakeDebugEntries = NwcWakeInbox.debugEntries()
        self.lastRevApplied = initial.rev
        self.lastNwcWakeStatusLogged = initial.nwc.lastWakeStatus
        self.lastNwcWakeSnapshot = nil
        self.lastReceiveNotificationKey = Self.receiveNotificationKey(initial.receive)

        rust.listenForUpdates(reconciler: self)
        observePushNotificationRegistration()
        observeNwcWakeInbox()
        rust.dispatch(action: .bootstrap)
        syncNwcWakeSnapshot()
        drainQueuedNwcWakeRequests()
    }

    convenience init() {
        self.init(storagePaths: AppStoragePreparer.prepareSynchronously())
    }

    nonisolated static func prepareStorage() async -> AppStoragePaths {
        await AppStoragePreparer.prepare()
    }

    nonisolated static func prepareStorageSynchronously() -> AppStoragePaths {
        AppStoragePreparer.prepareSynchronously()
    }

    nonisolated func reconcile(update: AppUpdate) {
        Task { @MainActor [weak self] in
            self?.apply(update: update)
        }
    }

    private func apply(update: AppUpdate) {
        switch update {
        case .fullState(let s):
            if s.rev <= lastRevApplied { return }
            recordNwcWakeDebugChanges(nextState: s)
            notifyIfReceiveCompleted(nextState: s)
            lastRevApplied = s.rev
            state = s
            nwcWakeRegistration.sync(state: s, signer: rust)
            syncNwcWakeSnapshot()
            // If a Lightning receive completed (e.g. while backgrounded), release the
            // background-execution assertion now that the core no longer needs to run.
            if !isAwaitingLightningReceive {
                endReceiveBackgroundTask()
            }
        case .haptic(let feedback):
            Haptics.play(feedback)
        }
    }

    private func recordNwcWakeDebugChanges(nextState: AppState) {
        if state.setup != .ready, nextState.setup == .ready, !nextState.nwc.pendingWakeRequests.isEmpty {
            NwcWakeInbox.appendDebug(
                source: "Rust",
                message: "Wallet ready; retrying \(nextState.nwc.pendingWakeRequests.count) pending NWC wake request\(nextState.nwc.pendingWakeRequests.count == 1 ? "" : "s")"
            )
        }

        let status = nextState.nwc.lastWakeStatus
        if status != lastNwcWakeStatusLogged {
            lastNwcWakeStatusLogged = status
            NwcWakeInbox.appendDebug(source: "Rust", message: status)
        }

        refreshNwcWakeDebugEntries()
    }

    private func notifyIfReceiveCompleted(nextState: AppState) {
        guard let key = Self.receiveNotificationKey(nextState.receive) else { return }
        guard key != lastReceiveNotificationKey else { return }

        lastReceiveNotificationKey = key
        schedulePaymentReceivedNotification(receive: nextState.receive)
    }

    private static func receiveNotificationKey(_ receive: ReceiveState) -> String? {
        guard receive.phase == .success else { return nil }

        if let paymentHash = receive.lightningPaymentHash, receive.lightningPaid {
            return "lightning:\(paymentHash)"
        }
        if let arkAddress = receive.arkAddress {
            return "ark:\(arkAddress):\(receive.amountSat)"
        }
        return nil
    }

    private func schedulePaymentReceivedNotification(receive: ReceiveState) {
        let content = UNMutableNotificationContent()
        content.title = "Payment received"
        if receive.amountSat > 0 {
            content.body = "Received \(receive.amountSat.formatted()) sats"
        } else {
            content.body = "Received payment"
        }
        content.sound = .default
        content.threadIdentifier = "wallet-receive"
        content.userInfo = [
            "type": "payment_received"
        ]

        let request = UNNotificationRequest(
            identifier: "payment-received-\(UUID().uuidString)",
            content: content,
            trigger: nil
        )

        UNUserNotificationCenter.current().add(request) { error in
            if let error {
                NSLog("RebelWallet failed to schedule receive notification: %@", String(describing: error))
            }
        }
    }

    /// True while we are showing a receive request and still waiting for the
    /// Lightning payment to be claimed. The Rust core must keep polling to supply
    /// the preimage, so the process needs to stay alive if backgrounded.
    var isAwaitingLightningReceive: Bool {
        state.receive.phase == .showingRequest
            && state.receive.lightningInvoice != nil
            && !state.receive.lightningPaid
    }

    /// Request a background-execution assertion so the Rust core keeps polling and
    /// can claim an in-flight Lightning receive after the app is backgrounded.
    /// iOS grants a limited window (~30s); we release the assertion as soon as the
    /// payment is claimed, the app returns to the foreground, or the window expires.
    func beginReceiveBackgroundTaskIfNeeded() {
        guard isAwaitingLightningReceive else { return }
        guard receiveBackgroundTask == .invalid else { return }
        receiveBackgroundTask = UIApplication.shared.beginBackgroundTask(withName: "LightningReceive") { [weak self] in
            self?.endReceiveBackgroundTask()
        }
    }

    func endReceiveBackgroundTask() {
        guard receiveBackgroundTask != .invalid else { return }
        UIApplication.shared.endBackgroundTask(receiveBackgroundTask)
        receiveBackgroundTask = .invalid
    }

    func dispatch(_ action: AppAction) {
        rust.dispatch(action: action)
    }

    func requestHaptic(_ feedback: HapticFeedback) {
        dispatch(.requestHaptic(feedback: feedback))
    }

    func unregisterNwcWakeConnection(_ connection: NwcConnection) {
        nwcWakeRegistration.unregister(state: state, connection: connection, signer: rust)
    }

    func registerNwcWakeConnectionForNwa(_ connection: NwcConnection) async throws {
        try await nwcWakeRegistration.registerNow(state: state, signer: rust, connection: connection)
    }

    func handleOpenURL(_ url: URL) {
        switch NwaWalletCreatedRequest.parse(url) {
        case .success(let request):
            pendingNwaWalletRequest = request
            if state.setup == .ready {
                dispatch(.pushScreen(screen: .nwc))
            }
        case .failure(.notNwa):
            return
        case .failure(let error):
            NwcWakeInbox.appendDebug(source: "App", message: "NWA request ignored: \(error.localizedDescription)")
            refreshNwcWakeDebugEntries()
        }
    }

    func dismissNwaWalletRequest(_ request: NwaWalletCreatedRequest) {
        guard pendingNwaWalletRequest?.id == request.id else { return }
        pendingNwaWalletRequest = nil
    }

    private func observePushNotificationRegistration() {
        let observer = NotificationCenter.default.addObserver(
            forName: PushNotificationEvents.registrationDidChange,
            object: nil,
            queue: .main
        ) { [weak self] notification in
            let status = notification.userInfo?[PushNotificationEvents.statusKey] as? String
            let deviceToken = notification.userInfo?[PushNotificationEvents.deviceTokenKey] as? String

            Task { @MainActor [weak self] in
                self?.dispatch(.setPushNotificationRegistration(
                    apnsDeviceToken: deviceToken,
                    registrationStatus: status ?? "Unknown"
                ))
            }
        }
        notificationObservers.append(observer)
    }

    private func observeNwcWakeInbox() {
        let observer = NotificationCenter.default.addObserver(
            forName: NwcWakeInboxEvents.didChange,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            Task { @MainActor [weak self] in
                self?.refreshNwcWakeDebugEntries()
                self?.drainQueuedNwcWakeRequests()
            }
        }
        notificationObservers.append(observer)
    }

    func drainQueuedNwcWakeRequests() {
        let requests = NwcWakeInbox.drain()
        guard !requests.isEmpty else { return }

        NwcWakeInbox.appendDebug(
            source: "App",
            message: "Drained \(requests.count) queued nwc_wake request\(requests.count == 1 ? "" : "s")"
        )
        refreshNwcWakeDebugEntries()
        dispatch(.processNwcWakeRequests(requests: requests.map {
            NwcWakeRequest(
                relay: $0.relay,
                eventId: $0.eventId,
                walletServicePubkey: $0.walletServicePubkey,
                receivedAt: $0.receivedAt
            )
        }))
    }

    func refreshNwcWakeDebugEntries() {
        nwcWakeDebugEntries = Array(NwcWakeInbox.debugEntries().reversed())
    }

    func clearNwcWakeDebugEntries() {
        NwcWakeInbox.clearDebugEntries()
        refreshNwcWakeDebugEntries()
    }

    private func syncNwcWakeSnapshot() {
        let snapshot = rust.nwcWakeSnapshotJson()
        guard snapshot != lastNwcWakeSnapshot else { return }
        lastNwcWakeSnapshot = snapshot
        NwcWakeInbox.saveSnapshot(snapshot)
    }

    func syncWalletForRefresh() async {
        if state.busy.syncingWallet {
            await waitForWalletSync()
            return
        }

        let startingRev = state.rev
        dispatch(.syncWallet)
        await waitForWalletSync(startingRev: startingRev)
    }

    private func waitForWalletSync(startingRev: UInt64? = nil) async {
        let timeout = Date().addingTimeInterval(60)
        var observedSync = state.busy.syncingWallet

        while Date() < timeout {
            if state.busy.syncingWallet {
                observedSync = true
            } else if observedSync {
                return
            } else if let startingRev, state.rev > startingRev {
                return
            }

            try? await Task.sleep(nanoseconds: 100_000_000)
        }
    }
}

struct NwaWalletCreatedRequest: Identifiable, Equatable {
    let id = UUID()
    let sourceURL: URL
    let name: String
    let appId: String
    let returnTo: URL
    let state: String
    let relay: String
    let budgetSat: UInt64
    let budgetInterval: NwcBudgetInterval
    let permissions: [NwcPermission]
    let createdAt = Date()

    var displayName: String {
        let trimmedName = name.trimmingCharacters(in: .whitespacesAndNewlines)
        if !trimmedName.isEmpty {
            return trimmedName
        }
        let trimmedAppId = appId.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmedAppId.isEmpty ? "External App" : trimmedAppId
    }

    static func parse(_ url: URL) -> Result<NwaWalletCreatedRequest, NwaWalletAuthError> {
        guard
            let components = URLComponents(url: url, resolvingAgainstBaseURL: false),
            let scheme = components.scheme?.lowercased()
        else {
            return .failure(.notNwa)
        }

        let isWalletAuthScheme = scheme == "nostr+walletauth" || scheme == "nostr+walletauth+rebelwallet"
        let isPrivateWalletScheme = scheme == "rebelwallet"
            && components.host?.lowercased() == "nwa"
            && components.path.trimmingCharacters(in: CharacterSet(charactersIn: "/")).lowercased() == "connect"
        guard isWalletAuthScheme || isPrivateWalletScheme else {
            return .failure(.notNwa)
        }

        let query = NwaQuery(components.queryItems ?? [])
        guard query.value("version") ?? "1" == "1" else {
            return .failure(.unsupportedVersion)
        }

        let hasClientPubkey = !(query.value("pubkey") ?? "").trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        let secretMode = (query.value("secret_mode") ?? (hasClientPubkey ? "client" : "wallet")).lowercased()
        guard secretMode == "wallet", !hasClientPubkey else {
            return .failure(.unsupportedSecretMode)
        }

        guard let returnToRaw = query.value("return_to"), let returnTo = URL(string: returnToRaw) else {
            return .failure(.missingReturnTo)
        }
        guard URLComponents(url: returnTo, resolvingAgainstBaseURL: false)?.fragment == nil else {
            return .failure(.invalidReturnTo)
        }
        guard let state = query.value("state")?.trimmingCharacters(in: .whitespacesAndNewlines), !state.isEmpty else {
            return .failure(.missingState)
        }

        let relays = query.values("relay")
        let relay = relays.joined(separator: "\n")
        let budgetSat = query.budgetSat() ?? 10_000
        let budgetInterval = NwcBudgetInterval.nwaValue(query.value("budget_renewal"))
        let permissions = NwcPermission.nwaPermissions(from: query.value("request_methods"))

        return .success(NwaWalletCreatedRequest(
            sourceURL: url,
            name: query.value("name") ?? query.value("appname") ?? "",
            appId: query.value("app_id") ?? "",
            returnTo: returnTo,
            state: state,
            relay: relay,
            budgetSat: budgetSat,
            budgetInterval: budgetInterval,
            permissions: permissions
        ))
    }

    func approvedCallback(nwcUri: String) -> URL? {
        callbackURL(items: [
            URLQueryItem(name: "state", value: state),
            URLQueryItem(name: "status", value: "approved"),
            URLQueryItem(name: "nwc_uri", value: nwcUri),
            URLQueryItem(name: "value", value: nwcUri)
        ])
    }

    func cancelledCallback() -> URL? {
        callbackURL(items: [
            URLQueryItem(name: "state", value: state),
            URLQueryItem(name: "status", value: "cancelled")
        ])
    }

    private func callbackURL(items: [URLQueryItem]) -> URL? {
        guard var callbackComponents = URLComponents(url: returnTo, resolvingAgainstBaseURL: false) else {
            return nil
        }
        var fragmentComponents = URLComponents()
        fragmentComponents.queryItems = items
        callbackComponents.percentEncodedFragment = fragmentComponents.percentEncodedQuery
        return callbackComponents.url
    }
}

enum NwaWalletAuthError: LocalizedError {
    case notNwa
    case unsupportedVersion
    case unsupportedSecretMode
    case missingReturnTo
    case invalidReturnTo
    case missingState

    var errorDescription: String? {
        switch self {
        case .notNwa:
            return "not an NWA URL"
        case .unsupportedVersion:
            return "unsupported NWA version"
        case .unsupportedSecretMode:
            return "only wallet-created secret mode is supported"
        case .missingReturnTo:
            return "missing return_to callback"
        case .invalidReturnTo:
            return "return_to must not include a fragment"
        case .missingState:
            return "missing state"
        }
    }
}

private struct NwaQuery {
    private let items: [URLQueryItem]

    init(_ items: [URLQueryItem]) {
        self.items = items
    }

    func value(_ name: String) -> String? {
        values(name).first
    }

    func values(_ name: String) -> [String] {
        items.compactMap { item in
            item.name == name ? item.value : nil
        }
    }

    func budgetSat() -> UInt64? {
        guard let rawMsat = value("max_amount"), let amountMsat = UInt64(rawMsat) else {
            return nil
        }
        return amountMsat.satsRoundedUpFromMsats()
    }
}

private extension UInt64 {
    func satsRoundedUpFromMsats() -> UInt64 {
        let sats = self / 1_000
        if self % 1_000 == 0 {
            return sats
        }
        return sats + 1
    }
}

private extension NwcBudgetInterval {
    static func nwaValue(_ value: String?) -> NwcBudgetInterval {
        switch value?.lowercased() {
        case "hourly":
            return .hourly
        case "weekly":
            return .weekly
        case "monthly":
            return .monthly
        default:
            return .daily
        }
    }
}

private extension NwcPermission {
    static func nwaPermissions(from value: String?) -> [NwcPermission] {
        let methods = Set((value ?? "")
            .split { character in
                character.isWhitespace || character == ","
            }
            .map { String($0).lowercased() })

        guard !methods.isEmpty else {
            return [
                .getInfo,
                .getBalance,
                .payInvoice,
                .payKeysend,
                .makeInvoice,
                .lookupInvoice,
                .listTransactions,
                .makeHoldInvoice,
                .cancelHoldInvoice,
                .settleHoldInvoice
            ]
        }

        var permissions: [NwcPermission] = []
        for method in methods {
            switch method {
            case "get_info":
                permissions.append(.getInfo)
            case "get_balance":
                permissions.append(.getBalance)
            case "pay_invoice":
                permissions.append(.payInvoice)
            case "pay_keysend":
                permissions.append(.payKeysend)
            case "make_invoice":
                permissions.append(.makeInvoice)
            case "lookup_invoice":
                permissions.append(.lookupInvoice)
            case "list_transactions":
                permissions.append(.listTransactions)
            case "make_hold_invoice":
                permissions.append(.makeHoldInvoice)
            case "cancel_hold_invoice":
                permissions.append(.cancelHoldInvoice)
            case "settle_hold_invoice":
                permissions.append(.settleHoldInvoice)
            default:
                continue
            }
        }

        if !permissions.contains(.getInfo) {
            permissions.append(.getInfo)
        }
        return permissions
    }
}

struct AppStoragePaths: Sendable {
    let dataDir: String
    let cacheDir: String
}

private enum AppStoragePreparer {
    static func prepare() async -> AppStoragePaths {
        await Task.detached(priority: .userInitiated) {
            prepareSynchronously()
        }.value
    }

    static func prepareSynchronously() -> AppStoragePaths {
        let fm = FileManager.default
        let legacyDataDirUrl = fm.urls(for: .applicationSupportDirectory, in: .userDomainMask).first!
        let legacyCacheDirUrl = fm.urls(for: .cachesDirectory, in: .userDomainMask).first!.appendingPathComponent("RebelWallet")
        let appGroupId = Bundle.main.object(forInfoDictionaryKey: "RebelWalletAppGroupIdentifier") as? String
        let appGroupRootUrl = appGroupId.flatMap { fm.containerURL(forSecurityApplicationGroupIdentifier: $0) }
        let sharedRootUrl = appGroupRootUrl?.appendingPathComponent("RustCore", isDirectory: true)
        let dataDirUrl = sharedRootUrl?.appendingPathComponent("ApplicationSupport", isDirectory: true) ?? legacyDataDirUrl
        let cacheDirUrl = sharedRootUrl?.appendingPathComponent("Caches", isDirectory: true) ?? legacyCacheDirUrl
        let dataDir = dataDirUrl.path
        let cacheDir = cacheDirUrl.path
        try? fm.createDirectory(at: dataDirUrl, withIntermediateDirectories: true)
        try? fm.createDirectory(at: cacheDirUrl, withIntermediateDirectories: true)
        if dataDirUrl != legacyDataDirUrl {
            Self.migrateLegacyData(from: legacyDataDirUrl, to: dataDirUrl)
        }
        Self.removeLegacyProfileCache(from: dataDirUrl)

        return AppStoragePaths(dataDir: dataDir, cacheDir: cacheDir)
    }

    private static func removeLegacyProfileCache(from dataDirUrl: URL) {
        let fm = FileManager.default
        for fileName in ["profiles.sqlite3", "profiles.sqlite3-wal", "profiles.sqlite3-shm"] {
            try? fm.removeItem(at: dataDirUrl.appendingPathComponent(fileName))
        }
        try? fm.removeItem(at: dataDirUrl.appendingPathComponent("profile_pictures"))
    }

    private static func migrateLegacyData(from legacyDataDirUrl: URL, to dataDirUrl: URL) {
        let fm = FileManager.default
        guard let items = try? fm.contentsOfDirectory(
            at: legacyDataDirUrl,
            includingPropertiesForKeys: [.isRegularFileKey],
            options: [.skipsHiddenFiles]
        ) else {
            return
        }

        for sourceUrl in items {
            let destinationUrl = dataDirUrl.appendingPathComponent(sourceUrl.lastPathComponent)
            guard !fm.fileExists(atPath: destinationUrl.path) else { continue }
            try? fm.copyItem(at: sourceUrl, to: destinationUrl)
        }
    }

}

final class KeychainSecretStore: SecretStore {
    private let service = "com.rebelwallet.app"
    private let accessGroup = KeychainSecretStore.keychainAccessGroup

    func getSecret(key: String) -> String? {
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

    func setSecret(key: String, value: String) -> Bool {
        let data = Data(value.utf8)
        var query = baseQuery(key: key)
        let update: [String: Any] = [kSecValueData as String: data]

        let status = SecItemUpdate(query as CFDictionary, update as CFDictionary)
        if status == errSecSuccess {
            return true
        }

        query[kSecValueData as String] = data
        query[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        return SecItemAdd(query as CFDictionary, nil) == errSecSuccess
    }

    func deleteSecret(key: String) -> Bool {
        SecItemDelete(baseQuery(key: key) as CFDictionary) == errSecSuccess
    }

    private func baseQuery(key: String) -> [String: Any] {
        var query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: key
        ]
        if let accessGroup {
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
