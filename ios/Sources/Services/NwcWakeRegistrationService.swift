import Foundation

@MainActor
final class NwcWakeRegistrationService {
    private let installIdKey = "RebelWalletNwcWakeInstallId"
    private var inFlightFingerprint: String?
    private var registeredFingerprint: String?
    private var failedFingerprint: String?

    func sync(state: AppState) {
        guard let serverURL = Self.serverURL else { return }
        guard let deviceToken = state.pushNotifications.apnsDeviceToken, !deviceToken.isEmpty else { return }
        guard !state.nwc.connections.isEmpty else { return }

        let fingerprint = Self.fingerprint(deviceToken: deviceToken, connections: state.nwc.connections)
        guard fingerprint != registeredFingerprint, fingerprint != inFlightFingerprint, fingerprint != failedFingerprint else {
            return
        }

        inFlightFingerprint = fingerprint
        let installId = installId()
        let bundleId = Bundle.main.bundleIdentifier ?? "com.nicktee.rebelwallet"
        let environment = Self.apnsEnvironment
        let connections = state.nwc.connections

        Task {
            do {
                for connection in connections {
                    try await Self.register(
                        serverURL: serverURL,
                        installId: installId,
                        deviceToken: deviceToken,
                        bundleId: bundleId,
                        environment: environment,
                        connection: connection
                    )
                }
                await MainActor.run {
                    self.registeredFingerprint = fingerprint
                    self.failedFingerprint = nil
                    self.inFlightFingerprint = nil
                    NwcWakeInbox.appendDebug(source: "App", message: "Registered \(connections.count) NWC wake connection\(connections.count == 1 ? "" : "s")")
                }
            } catch {
                await MainActor.run {
                    self.failedFingerprint = fingerprint
                    self.inFlightFingerprint = nil
                    NwcWakeInbox.appendDebug(source: "App", message: "NWC wake registration failed: \(error.localizedDescription)")
                }
            }
        }
    }

    static func uriWithWake(_ uri: String) -> String {
        guard
            let wakeURL = serverURL?.appendingPathComponent(".well-known/nostr/nwc-wake").absoluteString,
            var components = URLComponents(string: uri)
        else {
            return uri
        }

        var queryItems = components.queryItems ?? []
        if queryItems.contains(where: { $0.name == "wake" }) {
            return uri
        }
        queryItems.append(URLQueryItem(name: "wake", value: wakeURL))
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
        installId: String,
        deviceToken: String,
        bundleId: String,
        environment: String,
        connection: NwcConnection
    ) async throws {
        let url = serverURL.appendingPathComponent("register-apns-nwc")
        var request = URLRequest(url: url)
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try JSONEncoder().encode(RegisterApnsNwcPayload(
            id: installId,
            deviceToken: deviceToken,
            bundleId: bundleId,
            environment: environment,
            author: connection.clientPubkey,
            tagged: connection.servicePubkey,
            relay: connection.relay,
            name: connection.name,
            enabled: true
        ))

        let (_, response) = try await URLSession.shared.data(for: request)
        guard let http = response as? HTTPURLResponse, 200..<300 ~= http.statusCode else {
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
}

private struct RegisterApnsNwcPayload: Encodable {
    let id: String
    let deviceToken: String
    let bundleId: String
    let environment: String
    let author: String
    let tagged: String
    let relay: String
    let name: String
    let enabled: Bool

    enum CodingKeys: String, CodingKey {
        case id
        case deviceToken = "device_token"
        case bundleId = "bundle_id"
        case environment
        case author
        case tagged
        case relay
        case name
        case enabled
    }
}

private enum NwcWakeRegistrationError: LocalizedError {
    case serverRejected

    var errorDescription: String? {
        "wake server rejected registration"
    }
}
