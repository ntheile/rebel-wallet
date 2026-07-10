import Foundation

@MainActor
final class NwcWakeRegistrationService {
    private static let registrationRetryDelay: TimeInterval = 30

    private let installIdKey = "RebelWalletNwcWakeInstallId"
    private let pendingUnregistrationsKey = "RebelWalletPendingNwcWakeUnregistrations"
    private var inFlightFingerprint: String?
    private var registeredFingerprint: String?
    private var failedFingerprint: String?
    private var failedRetryAfter: Date?
    private var retryingPendingUnregistrations = false

    func sync(state: AppState, signer: FfiApp) {
        retryPendingUnregistrations(state: state, signer: signer)

        guard let serverURL = Self.serverURL else { return }
        guard let deviceToken = state.pushNotifications.apnsDeviceToken, !deviceToken.isEmpty else { return }
        guard !state.nwc.connections.isEmpty else { return }

        let fingerprint = Self.fingerprint(deviceToken: deviceToken, connections: state.nwc.connections)
        if fingerprint == failedFingerprint,
           let failedRetryAfter,
           Date() < failedRetryAfter {
            return
        }
        guard fingerprint != registeredFingerprint, fingerprint != inFlightFingerprint else {
            return
        }

        inFlightFingerprint = fingerprint
        let installId = installId()
        let bundleId = Bundle.main.bundleIdentifier ?? "com.rebelwallet.app"
        let environment = Self.apnsEnvironment
        let connections = state.nwc.connections

        Task {
            do {
                for connection in connections {
                    for relay in Self.relays(for: connection) {
                        try await Self.register(
                            serverURL: serverURL,
                            signer: signer,
                            installId: installId,
                            deviceToken: deviceToken,
                            bundleId: bundleId,
                            environment: environment,
                            registration: Self.registration(for: connection, relay: relay),
                            enabled: true
                        )
                    }
                }
                await MainActor.run {
                    self.registeredFingerprint = fingerprint
                    self.failedFingerprint = nil
                    self.failedRetryAfter = nil
                    self.inFlightFingerprint = nil
                    NwcWakeInbox.appendDebug(source: "App", message: "Registered \(connections.count) NWC wake connection\(connections.count == 1 ? "" : "s")")
                }
            } catch {
                await MainActor.run {
                    self.failedFingerprint = fingerprint
                    self.failedRetryAfter = Date().addingTimeInterval(Self.registrationRetryDelay)
                    self.inFlightFingerprint = nil
                    NwcWakeInbox.appendDebug(source: "App", message: "NWC wake registration failed: \(error.localizedDescription)")
                }
            }
        }
    }

    func unregister(state: AppState, connection: NwcConnection, signer: FfiApp) {
        let registrations = Self.registrations(for: connection)
        enqueuePendingUnregistrations(registrations)

        Task {
            do {
                try await unregisterPending(
                    registrations,
                    state: state,
                    signer: signer
                )
                await MainActor.run {
                    self.registeredFingerprint = nil
                    self.failedFingerprint = nil
                    self.failedRetryAfter = nil
                    NwcWakeInbox.appendDebug(source: "App", message: "Unregistered NWC wake connection \(connection.name)")
                }
            } catch {
                await MainActor.run {
                    NwcWakeInbox.appendDebug(source: "App", message: "NWC wake unregister failed: \(error.localizedDescription)")
                }
            }
        }
    }

    func unregisterNow(state: AppState, connection: NwcConnection, signer: FfiApp) async throws {
        let registrations = Self.registrations(for: connection)
        enqueuePendingUnregistrations(registrations)
        try await unregisterPending(registrations, state: state, signer: signer)
        registeredFingerprint = nil
        failedFingerprint = nil
        failedRetryAfter = nil
        NwcWakeInbox.appendDebug(source: "App", message: "Unregistered NWC wake connection \(connection.name)")
    }

    func registerNow(state: AppState, signer: FfiApp, connection: NwcConnection) async throws {
        guard let serverURL = Self.serverURL else {
            throw NwcWakeRegistrationError.serverUnavailable
        }
        guard let deviceToken = state.pushNotifications.apnsDeviceToken, !deviceToken.isEmpty else {
            throw NwcWakeRegistrationError.apnsTokenUnavailable
        }
        let installId = installId()
        let bundleId = Bundle.main.bundleIdentifier ?? "com.rebelwallet.app"
        let environment = Self.apnsEnvironment
        for relay in Self.relays(for: connection) {
            try await Self.register(
                serverURL: serverURL,
                signer: signer,
                installId: installId,
                deviceToken: deviceToken,
                bundleId: bundleId,
                environment: environment,
                registration: Self.registration(for: connection, relay: relay),
                enabled: true
            )
        }

        registeredFingerprint = nil
        failedFingerprint = nil
        failedRetryAfter = nil
        NwcWakeInbox.appendDebug(source: "App", message: "Registered NWC wake connection \(connection.name)")
    }

    static func uriWithConnectionMetadata(_ uri: String, lud16: String? = nil) -> String {
        guard var components = URLComponents(string: uri) else {
            return uri
        }

        var queryItems = components.queryItems ?? []
        let trimmedLud16 = lud16?.trimmingCharacters(in: .whitespacesAndNewlines)
        if let trimmedLud16, !trimmedLud16.isEmpty, !queryItems.contains(where: { $0.name == "lud16" }) {
            queryItems.append(URLQueryItem(name: "lud16", value: trimmedLud16))
        }
        components.queryItems = queryItems

        return components.string ?? uri
    }

    private func installId() -> String {
        if let id = UserDefaults.standard.string(forKey: installIdKey), !id.isEmpty {
            return id
        }

        let id = UUID().uuidString
        UserDefaults.standard.set(id, forKey: installIdKey)
        return id
    }

    private static var serverURL: URL? {
        guard let value = Bundle.main.object(forInfoDictionaryKey: "RebelWalletNwcWakeServerURL") as? String else {
            return nil
        }
        let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty, !trimmed.hasPrefix("$(") else {
            return nil
        }
        return URL(string: trimmed)
    }

    private static var apnsEnvironment: String {
        guard let value = Bundle.main.object(forInfoDictionaryKey: "RebelWalletApnsEnvironment") as? String else {
            return "sandbox"
        }
        return value == "production" ? "production" : "sandbox"
    }

    private static func register(
        serverURL: URL,
        signer: FfiApp,
        installId: String,
        deviceToken: String,
        bundleId: String,
        environment: String,
        registration: StoredNwcWakeRegistration,
        enabled: Bool
    ) async throws {
        let url = serverURL.appendingPathComponent("register-nwc-push")
        var request = URLRequest(url: url)
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        let body = try JSONEncoder().encode(RegisterNwcPushPayload(
            id: installId,
            pushService: "apns",
            pushToken: deviceToken,
            appId: bundleId,
            environment: environment,
            clientPubkey: registration.clientPubkey,
            walletServicePubkey: registration.walletServicePubkey,
            relay: registration.relay,
            name: registration.name,
            enabled: enabled
        ))
        request.httpBody = body
        guard
            let bodyJson = String(data: body, encoding: .utf8),
            let authHeader = signer.nwcPushRegistrationAuthHeader(
                url: url.absoluteString,
                bodyJson: bodyJson,
                walletServicePubkey: registration.walletServicePubkey
            )
        else {
            throw NwcWakeRegistrationError.authSigningFailed
        }
        request.setValue(authHeader, forHTTPHeaderField: "Authorization")

        let (_, response) = try await URLSession.shared.data(for: request)
        guard let http = response as? HTTPURLResponse, 200 ..< 300 ~= http.statusCode else {
            throw NwcWakeRegistrationError.serverRejected
        }
    }

    private static func fingerprint(deviceToken: String, connections: [NwcConnection]) -> String {
        let connectionFingerprint = connections
            .map { "\($0.id)|\($0.clientPubkey)|\($0.servicePubkey)|\($0.relay)|\($0.name)" }
            .sorted()
            .joined(separator: "\n")
        return "\(deviceToken)\n\(connectionFingerprint)"
    }

    private static func relays(for connection: NwcConnection) -> [String] {
        var seen = Set<String>()
        return connection.relay
            .components(separatedBy: CharacterSet.whitespacesAndNewlines.union(CharacterSet(charactersIn: ",")))
            .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
            .filter { !$0.isEmpty }
            .map { $0.hasSuffix("/") ? String($0.dropLast()) : $0 }
            .filter { relay in
                if seen.contains(relay) {
                    return false
                }
                seen.insert(relay)
                return true
            }
            .prefix(2)
            .map { String($0) }
    }

    private static func registration(
        for connection: NwcConnection,
        relay: String
    ) -> StoredNwcWakeRegistration {
        StoredNwcWakeRegistration(
            clientPubkey: connection.clientPubkey,
            walletServicePubkey: connection.servicePubkey,
            relay: relay,
            name: connection.name
        )
    }

    private static func registrations(for connection: NwcConnection) -> [StoredNwcWakeRegistration] {
        relays(for: connection).map { registration(for: connection, relay: $0) }
    }

    private func unregisterPending(
        _ registrations: [StoredNwcWakeRegistration],
        state: AppState,
        signer: FfiApp
    ) async throws {
        guard let serverURL = Self.serverURL else {
            throw NwcWakeRegistrationError.serverUnavailable
        }
        let installId = installId()
        let bundleId = Bundle.main.bundleIdentifier ?? "com.rebelwallet.app"
        let environment = Self.apnsEnvironment
        let deviceToken = state.pushNotifications.apnsDeviceToken ?? ""
        for registration in registrations {
            try await Self.register(
                serverURL: serverURL,
                signer: signer,
                installId: installId,
                deviceToken: deviceToken,
                bundleId: bundleId,
                environment: environment,
                registration: registration,
                enabled: false
            )
            removePendingUnregistration(registration)
        }
    }

    private func retryPendingUnregistrations(state: AppState, signer: FfiApp) {
        guard !retryingPendingUnregistrations else { return }
        let pending = pendingUnregistrations()
        guard !pending.isEmpty else { return }

        retryingPendingUnregistrations = true
        Task {
            defer { retryingPendingUnregistrations = false }
            do {
                try await unregisterPending(pending, state: state, signer: signer)
                NwcWakeInbox.appendDebug(
                    source: "App",
                    message: "Retried \(pending.count) pending NWC wake unregistration\(pending.count == 1 ? "" : "s")"
                )
            } catch {
                NwcWakeInbox.appendDebug(
                    source: "App",
                    message: "Pending NWC wake unregister retry failed: \(error.localizedDescription)"
                )
            }
        }
    }

    private func pendingUnregistrations() -> [StoredNwcWakeRegistration] {
        guard
            let data = UserDefaults.standard.data(forKey: pendingUnregistrationsKey),
            let registrations = try? JSONDecoder().decode([StoredNwcWakeRegistration].self, from: data)
        else {
            return []
        }
        return registrations
    }

    private func enqueuePendingUnregistrations(_ registrations: [StoredNwcWakeRegistration]) {
        var pending = pendingUnregistrations()
        for registration in registrations where !pending.contains(registration) {
            pending.append(registration)
        }
        savePendingUnregistrations(pending)
    }

    private func removePendingUnregistration(_ registration: StoredNwcWakeRegistration) {
        savePendingUnregistrations(pendingUnregistrations().filter { $0 != registration })
    }

    private func savePendingUnregistrations(_ registrations: [StoredNwcWakeRegistration]) {
        if registrations.isEmpty {
            UserDefaults.standard.removeObject(forKey: pendingUnregistrationsKey)
            return
        }
        guard let data = try? JSONEncoder().encode(registrations) else { return }
        UserDefaults.standard.set(data, forKey: pendingUnregistrationsKey)
    }
}

private struct StoredNwcWakeRegistration: Codable, Equatable {
    let clientPubkey: String
    let walletServicePubkey: String
    let relay: String
    let name: String
}

private struct RegisterNwcPushPayload: Encodable {
    let id: String
    let pushService: String
    let pushToken: String
    let appId: String
    let environment: String
    let clientPubkey: String
    let walletServicePubkey: String
    let relay: String
    let name: String
    let enabled: Bool

    enum CodingKeys: String, CodingKey {
        case id
        case pushService = "push_service"
        case pushToken = "push_token"
        case appId = "app_id"
        case environment
        case clientPubkey = "client_pubkey"
        case walletServicePubkey = "wallet_service_pubkey"
        case relay
        case name
        case enabled
    }
}

private enum NwcWakeRegistrationError: LocalizedError {
    case serverUnavailable
    case apnsTokenUnavailable
    case authSigningFailed
    case serverRejected

    var errorDescription: String? {
        switch self {
        case .serverUnavailable:
            "wake server is not configured"
        case .apnsTokenUnavailable:
            "APNs device token is not available"
        case .authSigningFailed:
            "could not sign NWC wake registration"
        case .serverRejected:
            "wake server rejected registration"
        }
    }
}
