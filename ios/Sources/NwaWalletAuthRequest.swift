import Foundation

struct NwaWalletAuthRequest: Identifiable, Equatable {
    private static let maximumRequestLength = 8192
    private static let maximumCallbackLength = 2048
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
            budgetSat = maximumAmountMsat / 1000
        } else {
            budgetSat = 10000
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
            URLQueryItem(name: "wallet_pubkey", value: walletPubkey),
        ]
        items.append(contentsOf: relays.map { URLQueryItem(name: "relay", value: $0) })
        if let lud16, !lud16.isEmpty {
            items.append(URLQueryItem(name: "lud16", value: lud16))
        }
        return callbackURL(items: items)
    }

    func cancelledCallback() -> URL? {
        callbackURL(items: stateQueryItems + [
            URLQueryItem(name: "status", value: "cancelled"),
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
                .listTransactions,
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
