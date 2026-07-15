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
    var pendingNwaWalletRequest: NwaWalletAuthRequest?
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

    func openNwaCallback(_ callback: URL) async -> Bool {
        let callbackTarget = Self.nwaCallbackTargetDescription(callback)
        let bundleId = Bundle.main.bundleIdentifier ?? "unknown"
        let universalLinksOnly = callback.scheme?.lowercased() == "https"
        NwcWakeInbox.appendDebug(
            source: "App",
            message: "NWA callback open requested wallet_bundle=\(bundleId) target=\(callbackTarget) universal_links_only=\(universalLinksOnly)"
        )

        let opened = await withCheckedContinuation { continuation in
            let completion: (Bool) -> Void = { opened in
                continuation.resume(returning: opened)
            }
            if universalLinksOnly {
                UIApplication.shared.open(
                    callback,
                    options: [.universalLinksOnly: true],
                    completionHandler: completion
                )
            } else {
                UIApplication.shared.open(callback, completionHandler: completion)
            }
        }

        NwcWakeInbox.appendDebug(
            source: "App",
            message: "NWA callback open result opened=\(opened) target=\(callbackTarget)"
        )
        return opened
    }

    func handleOpenURL(_ url: URL) {
        switch NwaWalletAuthRequest.parse(url) {
        case .success(let request):
            let bundleId = Bundle.main.bundleIdentifier ?? "unknown"
            let relayCount = request.relay.split(whereSeparator: { $0.isNewline }).count
            NwcWakeInbox.appendDebug(
                source: "App",
                message: "NWA request accepted wallet_bundle=\(bundleId) client_pubkey=\(request.clientPubkey) callback=\(request.callbackTargetDescription) budget_sat=\(request.budgetSat) interval=\(String(describing: request.budgetInterval)) relays=\(relayCount) permissions=\(request.permissions.count)"
            )
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

    private static func nwaCallbackTargetDescription(_ callback: URL) -> String {
        guard
            let components = URLComponents(url: callback, resolvingAgainstBaseURL: false),
            let scheme = components.scheme?.lowercased(),
            let host = components.host?.lowercased()
        else {
            return "invalid"
        }

        let port = components.port.map { ":\($0)" } ?? ""
        return "\(scheme)://\(host)\(port)\(components.path)"
    }

    func dismissNwaWalletRequest(_ request: NwaWalletAuthRequest) {
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

struct NwaWalletAuthRequest: Identifiable, Equatable {
    private static let maximumRequestLength = 8_192
    private static let maximumCallbackLength = 2_048
    private static let minimumStateLength = 32
    private static let maximumStateLength = 256

    let id = UUID()
    let sourceURL: URL
    let clientPubkey: String
    let name: String
    let returnTo: URL?
    let state: String?
    let relay: String
    let budgetSat: UInt64
    let budgetInterval: NwcBudgetInterval
    let permissions: [NwcPermission]
    let expiresAt: UInt64?
    let createdAt = Date()

    var displayName: String {
        let trimmedName = name.trimmingCharacters(in: .whitespacesAndNewlines)
        if !trimmedName.isEmpty {
            return trimmedName
        }
        return "External App"
    }

    var requestingAppDescription: String? {
        returnTo?.host
    }

    var callbackTargetDescription: String {
        guard
            let returnTo,
            let components = URLComponents(url: returnTo, resolvingAgainstBaseURL: false),
            let scheme = components.scheme?.lowercased(),
            let host = components.host?.lowercased()
        else {
            return "none"
        }

        let port = components.port.map { ":\($0)" } ?? ""
        return "\(scheme)://\(host)\(port)\(components.path)"
    }

    static func parse(_ url: URL) -> Result<NwaWalletAuthRequest, NwaWalletAuthError> {
        guard url.absoluteString.utf8.count <= maximumRequestLength else {
            return .failure(.requestTooLarge)
        }
        guard
            let components = URLComponents(url: url, resolvingAgainstBaseURL: false),
            let scheme = components.scheme?.lowercased()
        else {
            return .failure(.notNwa)
        }

        let isWalletAuthScheme = scheme == "nostr+walletauth" || scheme == "nostr+walletauth+rebelwallet"
        guard isWalletAuthScheme else {
            return .failure(.notNwa)
        }

        let clientPubkey = components.host?.trimmingCharacters(in: .whitespacesAndNewlines).lowercased() ?? ""
        guard clientPubkey.count == 64, clientPubkey.allSatisfy({ $0.isHexDigit }) else {
            return .failure(.invalidClientPubkey)
        }

        // NWA clients use URLSearchParams, whose query encoding represents
        // spaces as "+". URLComponents does not apply that form-URL decoding,
        // so normalize raw plus separators before decoding percent escapes.
        var formComponents = components
        formComponents.percentEncodedQuery = components.percentEncodedQuery?
            .replacingOccurrences(of: "+", with: "%20")
        let query = NwaQuery(formComponents.queryItems ?? [])
        guard !query.hasDuplicateSingleValueParameters(repeatable: ["relay"]) else {
            return .failure(.duplicateParameter)
        }
        guard (query.value("version") ?? "1") == "1" else {
            return .failure(.unsupportedVersion)
        }

        guard query.value("pubkey") == nil else {
            return .failure(.invalidClientPubkey)
        }
        guard (query.value("secret_mode") ?? "client").lowercased() == "client" else {
            return .failure(.unsupportedSecretMode)
        }

        guard (query.value("response_mode") ?? "relay").lowercased() == "relay" else {
            return .failure(.unsupportedResponseMode)
        }

        var returnTo: URL?
        var state: String?
        if let returnToRaw = query.value("return_to") {
            let requestState = query.value("state")?.trimmingCharacters(in: .whitespacesAndNewlines)
            let hasValidState: Bool
            if let requestState {
                hasValidState = requestState.utf8.count >= minimumStateLength
                    && requestState.utf8.count <= maximumStateLength
            } else {
                hasValidState = true
            }
            if
                returnToRaw.utf8.count <= maximumCallbackLength,
                let callback = URL(string: returnToRaw),
                isAllowedCallback(callback),
                hasValidState
            {
                returnTo = callback
                state = requestState
            }
        }

        var expiresAt: UInt64?
        if let expiresAtRaw = query.value("expires_at") {
            guard
                let parsedExpiresAt = UInt64(expiresAtRaw),
                parsedExpiresAt > UInt64(Date().timeIntervalSince1970)
            else {
                return .failure(.expiredRequest)
            }
            expiresAt = parsedExpiresAt
        }

        let relays = query.values("relay")
        guard !relays.isEmpty else {
            return .failure(.missingRelay)
        }
        let relay = relays.joined(separator: "\n")
        let budgetSat: UInt64
        if let maximumAmountMsat = query.value("max_amount") {
            guard let maximumAmountMsat = UInt64(maximumAmountMsat) else {
                return .failure(.invalidMaxAmount)
            }
            budgetSat = maximumAmountMsat / 1_000
        } else {
            budgetSat = 10_000
        }
        let budgetInterval = NwcBudgetInterval.nwaValue(query.value("budget_renewal"))
        let permissions = NwcPermission.nwaPermissions(from: query.value("request_methods"))

        return .success(NwaWalletAuthRequest(
            sourceURL: url,
            clientPubkey: clientPubkey,
            name: query.value("name") ?? query.value("appname") ?? "",
            returnTo: returnTo,
            state: state,
            relay: relay,
            budgetSat: budgetSat,
            budgetInterval: budgetInterval,
            permissions: permissions,
            expiresAt: expiresAt
        ))
    }

    func approvedCallback(walletPubkey: String, relays: [String], lud16: String?) -> URL? {
        var items = stateQueryItems + [
            URLQueryItem(name: "status", value: "approved"),
            URLQueryItem(name: "wallet_pubkey", value: walletPubkey)
        ]
        items.append(contentsOf: relays.map { URLQueryItem(name: "relay", value: $0) })
        if let lud16, !lud16.isEmpty {
            items.append(URLQueryItem(name: "lud16", value: lud16))
        }
        return callbackURL(items: items)
    }

    func cancelledCallback() -> URL? {
        callbackURL(items: stateQueryItems + [
            URLQueryItem(name: "status", value: "cancelled")
        ])
    }

    private var stateQueryItems: [URLQueryItem] {
        guard let state, !state.isEmpty else { return [] }
        return [URLQueryItem(name: "state", value: state)]
    }

    private func callbackURL(items: [URLQueryItem]) -> URL? {
        guard let returnTo, var callbackComponents = URLComponents(url: returnTo, resolvingAgainstBaseURL: false) else {
            return nil
        }
        var fragmentComponents = URLComponents()
        fragmentComponents.queryItems = items
        callbackComponents.percentEncodedFragment = fragmentComponents.percentEncodedQuery
        return callbackComponents.url
    }

    private static func isAllowedCallback(_ callback: URL) -> Bool {
        guard
            let callbackComponents = URLComponents(url: callback, resolvingAgainstBaseURL: false),
            let callbackScheme = callbackComponents.scheme?.lowercased(),
            callbackComponents.user == nil,
            callbackComponents.password == nil,
            callbackComponents.fragment == nil
        else {
            return false
        }

        if callbackScheme == "https" {
            return isVerifiedHTTPSCallback(callbackComponents)
        }

        let blockedSchemes = Set([
            "http", "file", "data", "javascript", "about", "blob",
            "nostr+walletauth", "nostr+walletauth+rebelwallet",
        ])
        guard
            !blockedSchemes.contains(callbackScheme),
            callbackComponents.port == nil,
            callbackComponents.host != nil || !callbackComponents.path.isEmpty
        else {
            return false
        }
        return true
    }

    private static func isVerifiedHTTPSCallback(_ callbackComponents: URLComponents) -> Bool {
        guard
            let callbackHost = callbackComponents.host?.lowercased(),
            isPublicDomain(callbackHost),
            callbackComponents.port == nil || callbackComponents.port == 443,
            !callbackComponents.path.isEmpty
        else {
            return false
        }
        return true
    }

    private static func isPublicDomain(_ host: String) -> Bool {
        guard host.contains("."), !host.hasSuffix(".local"), host != "localhost", !host.contains(":") else {
            return false
        }
        let parts = host.split(separator: ".", omittingEmptySubsequences: false)
        let isIPv4Address = parts.count == 4 && parts.allSatisfy { UInt8($0) != nil }
        return !isIPv4Address
    }
}

enum NwaWalletAuthError: LocalizedError {
    case notNwa
    case invalidClientPubkey
    case unsupportedVersion
    case unsupportedSecretMode
    case unsupportedResponseMode
    case duplicateParameter
    case requestTooLarge
    case missingRelay
    case invalidMaxAmount
    case expiredRequest

    var errorDescription: String? {
        switch self {
        case .notNwa:
            return "not an NWA URL"
        case .invalidClientPubkey:
            return "NWA requires a valid client public key in the URI authority"
        case .unsupportedVersion:
            return "unsupported NWA version"
        case .unsupportedSecretMode:
            return "only client-created secret mode is supported"
        case .unsupportedResponseMode:
            return "only relay response mode is supported"
        case .duplicateParameter:
            return "duplicate NWA parameter"
        case .requestTooLarge:
            return "NWA request is too large"
        case .missingRelay:
            return "at least one relay is required"
        case .invalidMaxAmount:
            return "max_amount must be an unsigned millisatoshi amount"
        case .expiredRequest:
            return "NWA request has expired"
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

    func hasDuplicateSingleValueParameters(repeatable: Set<String>) -> Bool {
        var seen = Set<String>()
        for item in items where !repeatable.contains(item.name) {
            if !seen.insert(item.name).inserted {
                return true
            }
        }
        return false
    }

}

private extension NwcBudgetInterval {
    static func nwaValue(_ value: String?) -> NwcBudgetInterval {
        switch value?.lowercased() {
        case nil, "", "never":
            return .never
        case "hourly":
            return .hourly
        case "daily":
            return .daily
        case "weekly":
            return .weekly
        case "monthly":
            return .monthly
        case "yearly":
            return .yearly
        default:
            return .never
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
                .makeInvoice,
                .lookupInvoice,
                .listTransactions
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
            case "make_invoice":
                permissions.append(.makeInvoice)
            case "lookup_invoice":
                permissions.append(.lookupInvoice)
            case "list_transactions":
                permissions.append(.listTransactions)
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
