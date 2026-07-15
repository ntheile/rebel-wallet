import SwiftUI

struct NwaWalletAuthApprovalView: View {
    @Bindable var manager: AppManager
    @Environment(\.dismiss) private var dismiss
    @Environment(\.walletAccent) private var walletAccent
    @FocusState private var focusedField: NwaApprovalField?
    let request: NwaWalletAuthRequest
    @State private var relay = ""
    @State private var budgetText = ""
    @State private var budgetInterval: NwcBudgetInterval = .daily
    @State private var permissionPreset: NwcPermissionPreset = .fullAccess
    @State private var selectedPermissions = Set<NwcPermission>()
    @State private var initialized = false
    @State private var editing = false
    @State private var customRelayDraft: NwcCustomRelayDraft?
    @State private var approving = false
    @State private var approvedConnection: NwcConnection?
    @State private var errorMessage: String?

    private var relays: [String] {
        nwcRelayURLs(relay)
    }

    private var relayStorage: String {
        encodeNwcRelayURLs(relays)
    }

    private var parsedBudget: UInt64? {
        let cleaned = budgetText.filter(\.isNumber)
        if cleaned.isEmpty {
            return 0
        }
        return UInt64(cleaned)
    }

    private var permissionsForCreate: [NwcPermission] {
        switch permissionPreset {
        case .fullAccess, .readOnly:
            return permissionPreset.permissions
        case .custom:
            return selectedPermissions.sortedForDisplay
        }
    }

    private var canApprove: Bool {
        parsedBudget != nil
            && !relays.isEmpty
            && manager.state.setup == .ready
    }

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 18) {
                    NwcConnectionVisualization()
                        .frame(maxWidth: .infinity)

                    SettingsCard(title: "Nostr Wallet Auth") {
                        VStack(alignment: .leading, spacing: 16) {
                            HStack(alignment: .top, spacing: 12) {
                                VStack(alignment: .leading, spacing: 6) {
                                    Text(request.displayName)
                                        .font(.title2.bold())
                                        .foregroundStyle(primaryText)
                                    Text("wants to connect to this wallet")
                                        .font(.subheadline)
                                        .foregroundStyle(mutedText)
                                    if let requestingAppDescription = request.requestingAppDescription {
                                        Text(requestingAppDescription)
                                            .font(.caption.monospaced())
                                            .foregroundStyle(mutedText)
                                    }
                                }
                                Spacer(minLength: 12)
                                Button(editing ? "Done" : "Edit") {
                                    focusedField = nil
                                    editing.toggle()
                                    manager.requestHaptic(.selection)
                                }
                                .font(.subheadline.bold())
                                .foregroundStyle(walletAccent)
                                .disabled(approving)
                            }

                            if editing {
                                VStack(alignment: .leading, spacing: 14) {
                                    VStack(alignment: .leading, spacing: 10) {
                                        Text("Budget")
                                            .font(.caption.bold())
                                            .foregroundStyle(mutedText)

                                        HStack(alignment: .firstTextBaseline, spacing: 8) {
                                            TextField("", text: $budgetText)
                                                .keyboardType(.numberPad)
                                                .focused($focusedField, equals: .budget)
                                                .multilineTextAlignment(.center)
                                                .font(.system(size: 34, weight: .light))
                                                .foregroundStyle(primaryText)
                                                .frame(minWidth: 90)
                                                .onChange(of: budgetText) { _, newValue in
                                                    let formatted = formatBudgetInput(newValue)
                                                    if formatted != newValue {
                                                        budgetText = formatted
                                                    }
                                                }

                                            Text("sats")
                                                .font(.subheadline.bold())
                                                .foregroundStyle(mutedText)
                                                .frame(width: 42, alignment: .leading)
                                        }
                                        .padding(.horizontal, 18)
                                        .padding(.vertical, 12)
                                        .background(Color.black, in: RoundedRectangle(cornerRadius: 8))
                                        .overlay(RoundedRectangle(cornerRadius: 8).stroke(Color.white.opacity(0.18)))

                                        HStack(spacing: 10) {
                                            ForEach([5000, 10000, 500_000], id: \.self) { amount in
                                                Button {
                                                    budgetText = formatBudgetInput("\(amount)")
                                                    focusedField = nil
                                                    manager.requestHaptic(.selection)
                                                } label: {
                                                    Text(compactSats(amount))
                                                        .font(.caption.bold())
                                                        .frame(maxWidth: .infinity)
                                                }
                                                .buttonStyle(SecondaryButtonStyle())
                                            }
                                        }
                                    }

                                    VStack(alignment: .leading, spacing: 10) {
                                        Text("Interval")
                                            .font(.caption.bold())
                                            .foregroundStyle(mutedText)

                                        Picker("Interval", selection: $budgetInterval) {
                                            ForEach(NwcBudgetInterval.createOptions, id: \.self) { interval in
                                                Text(interval.title)
                                                    .tag(interval)
                                            }
                                        }
                                        .pickerStyle(.segmented)
                                    }

                                    VStack(alignment: .leading, spacing: 10) {
                                        Text("Relays")
                                            .font(.caption.bold())
                                            .foregroundStyle(mutedText)

                                        NwcRelayPresetMenu(
                                            selection: NwcRelayPreset.matching(relay),
                                            relay: relay,
                                            select: selectRelayPreset,
                                            editCustom: editCustomRelay
                                        )
                                    }

                                    VStack(alignment: .leading, spacing: 10) {
                                        Text("Permissions")
                                            .font(.caption.bold())
                                            .foregroundStyle(mutedText)

                                        NwcPermissionPresetMenu(
                                            selection: permissionPreset,
                                            select: selectPermissionPreset
                                        )

                                        if permissionPreset == .custom {
                                            HStack {
                                                Spacer()

                                                Button("All") {
                                                    selectedPermissions = Set(NwcPermission.createOptions)
                                                    manager.requestHaptic(.selection)
                                                }
                                                .font(.caption.bold())
                                                .foregroundStyle(rebelGreen)

                                                Button("None") {
                                                    selectedPermissions.removeAll()
                                                    manager.requestHaptic(.selection)
                                                }
                                                .font(.caption.bold())
                                                .foregroundStyle(mutedText)
                                            }

                                            VStack(spacing: 10) {
                                                ForEach(NwcPermission.createOptions, id: \.self) { permission in
                                                    NwcPermissionToggleRow(
                                                        icon: permission.icon,
                                                        title: permission.title,
                                                        caption: permission.methodName,
                                                        color: permission.color(walletAccent: walletAccent),
                                                        isOn: Binding(
                                                            get: { selectedPermissions.contains(permission) },
                                                            set: { enabled in
                                                                if enabled {
                                                                    selectedPermissions.insert(permission)
                                                                } else {
                                                                    selectedPermissions.remove(permission)
                                                                }
                                                            }
                                                        )
                                                    )
                                                }
                                            }
                                        }
                                    }
                                }
                            } else {
                                VStack(alignment: .leading, spacing: 12) {
                                    VStack(alignment: .leading, spacing: 8) {
                                        NwaPolicyRow(icon: "bolt.fill", title: "Budget", value: "\(parsedBudget?.formatted() ?? request.budgetSat.formatted()) sats")
                                        NwaPolicyRow(icon: "calendar", title: "Interval", value: budgetInterval.title)
                                        NwaPolicyRow(icon: "antenna.radiowaves.left.and.right", title: "Relays", value: relays.joined(separator: "\n"))
                                    }

                                    VStack(alignment: .leading, spacing: 8) {
                                        Text("Permissions")
                                            .font(.caption.bold())
                                            .foregroundStyle(mutedText)
                                        LazyVGrid(columns: [GridItem(.adaptive(minimum: 92), spacing: 8)], alignment: .leading, spacing: 8) {
                                            ForEach(permissionsForCreate.sortedForDisplay, id: \.self) { permission in
                                                NwcPolicyPill(text: permission.shortTitle, color: permission.color(walletAccent: walletAccent))
                                            }
                                        }
                                    }
                                }
                            }

                            HStack(alignment: .top, spacing: 10) {
                                Image(systemName: "exclamationmark.triangle.fill")
                                    .foregroundStyle(Color.yellow)
                                Text("The requesting app keeps its NWC secret. Rebel receives only its public key and returns public connection details.")
                                    .font(.caption)
                                    .foregroundStyle(mutedText)
                            }
                            .padding(12)
                            .background(Color.yellow.opacity(0.10), in: RoundedRectangle(cornerRadius: 8))
                            .overlay(RoundedRectangle(cornerRadius: 8).stroke(Color.yellow.opacity(0.25)))

                            if let errorMessage {
                                Text(errorMessage)
                                    .font(.caption)
                                    .foregroundStyle(rebelRed)
                            }

                            Button {
                                Task {
                                    if let approvedConnection {
                                        await returnToRequestingApp(approvedConnection)
                                    } else {
                                        await approve()
                                    }
                                }
                            } label: {
                                HStack {
                                    if approving {
                                        ProgressView()
                                            .tint(.white)
                                    }
                                    Text(approving ? "Connecting..." : approvedConnection == nil ? "Connect" : "Return to App")
                                }
                                .frame(maxWidth: .infinity)
                            }
                            .buttonStyle(PrimaryButtonStyle(color: walletAccent))
                            .disabled(approving || !canApprove)

                            Button {
                                cancel()
                            } label: {
                                Text(approvedConnection == nil ? "Cancel" : "Done")
                                    .frame(maxWidth: .infinity)
                            }
                            .buttonStyle(SecondaryButtonStyle())
                            .disabled(approving)
                        }
                        .padding(14)
                    }
                }
                .padding(16)
            }
            .navigationTitle("Connect App")
            .toolbar {
                ToolbarItem(placement: .topBarTrailing) {
                    Button {
                        cancel()
                    } label: {
                        Image(systemName: "xmark")
                    }
                    .disabled(approving)
                    .accessibilityLabel("Cancel NWA request")
                }
            }
            .background(pageBackground)
            .foregroundStyle(primaryText)
            .contentShape(Rectangle())
            .onTapGesture {
                focusedField = nil
            }
            .scrollDismissesKeyboard(.interactively)
            .onAppear {
                initializeEditablePolicy()
            }
            .sheet(item: $customRelayDraft) { draft in
                NwcCustomRelaySheet(relay: $relay, initialRelay: draft.url)
                    .presentationDetents([.height(260), .medium])
            }
        }
    }

    private func approve() async {
        if let expiresAt = request.expiresAt,
           expiresAt <= UInt64(Date().timeIntervalSince1970)
        {
            errorMessage = "This connection request has expired."
            return
        }
        guard manager.state.setup == .ready else {
            errorMessage = "Open or create the wallet before connecting an app."
            return
        }
        guard !relays.isEmpty else {
            errorMessage = "The request did not include a valid relay."
            return
        }
        guard let parsedBudget else {
            errorMessage = "Enter a valid budget."
            return
        }

        approving = true
        errorMessage = nil
        let existingIds = Set(manager.state.nwc.connections.map(\.id))
        let effectivePermissions = permissionsForCreate
        NwcWakeInbox.appendDebug(
            source: "App",
            message: "NWA approval settings callback=\(request.callbackTargetDescription) budget_sat=\(parsedBudget) interval=\(budgetInterval.title.lowercased()) relays=\(relays.joined(separator: ",")) permissions=\(effectivePermissions.map(\.methodName).joined(separator: ","))"
        )
        manager.dispatch(.authorizeNwcConnection(
            name: request.displayName,
            relay: relayStorage,
            clientPubkey: request.clientPubkey,
            budgetSat: parsedBudget,
            budgetInterval: budgetInterval,
            permissions: effectivePermissions,
            expiresAt: request.expiresAt
        ))

        do {
            let connection = try await waitForCreatedConnection(existingIds: existingIds)
            try await manager.registerNwcWakeConnectionForNwa(connection)
            approvedConnection = connection
            await returnToRequestingApp(connection)
        } catch {
            await MainActor.run {
                approving = false
                errorMessage = error.localizedDescription
            }
        }
    }

    private func returnToRequestingApp(_ connection: NwcConnection) async {
        approving = true
        errorMessage = nil
        guard request.returnTo != nil else {
            manager.dismissNwaWalletRequest(request)
            dismiss()
            return
        }
        guard let callback = request.approvedCallback(
            walletPubkey: connection.servicePubkey,
            relays: relays,
            lud16: manager.state.lightningAddress.address
        ) else {
            approving = false
            errorMessage = NwaApprovalError.invalidCallback.localizedDescription
            return
        }
        guard await manager.openNwaCallback(callback) else {
            NwcWakeInbox.appendDebug(
                source: "App",
                message: "NWA connection approved but callback could not be opened"
            )
            approving = false
            errorMessage = "The connection was approved, but the requesting app could not be reopened. Return to it manually or retry."
            return
        }
        manager.dismissNwaWalletRequest(request)
        dismiss()
    }

    private func initializeEditablePolicy() {
        guard !initialized else { return }
        initialized = true

        let requestedRelays = nwcRelayURLs(request.relay)
        let defaultRelays = nwcRelayURLs(manager.state.nwc.defaultRelay)
        relay = encodeNwcRelayURLs(requestedRelays.isEmpty ? defaultRelays : requestedRelays)
        budgetText = formatBudgetInput("\(request.budgetSat)")
        budgetInterval = request.budgetInterval
        selectedPermissions = Set(request.permissions)
        permissionPreset = NwcPermissionPreset.matching(request.permissions)
    }

    private func selectRelayPreset(_ preset: NwcRelayPreset?) {
        if let preset {
            relay = encodeNwcRelayURLs(preset.urls)
        }
        focusedField = nil
        manager.requestHaptic(.selection)
    }

    private func editCustomRelay() {
        focusedField = nil
        customRelayDraft = NwcCustomRelayDraft(url: relay)
        manager.requestHaptic(.selection)
    }

    private func selectPermissionPreset(_ preset: NwcPermissionPreset) {
        permissionPreset = preset
        if preset != .custom {
            selectedPermissions = Set(preset.permissions)
        }
        manager.requestHaptic(.selection)
    }

    private func formatBudgetInput(_ value: String) -> String {
        let digits = value.filter(\.isNumber)
        guard !digits.isEmpty else {
            return ""
        }

        var output = ""
        let reversedDigits = Array(digits.reversed())
        for (index, character) in reversedDigits.enumerated() {
            if index > 0 && index.isMultiple(of: 3) {
                output.append(",")
            }
            output.append(character)
        }
        return String(output.reversed())
    }

    private func cancel() {
        if approvedConnection != nil {
            manager.dismissNwaWalletRequest(request)
            dismiss()
            return
        }
        Task {
            if let callback = request.cancelledCallback() {
                _ = await manager.openNwaCallback(callback)
            }
            await MainActor.run {
                manager.dismissNwaWalletRequest(request)
                dismiss()
            }
        }
    }

    private func waitForCreatedConnection(existingIds: Set<String>) async throws -> NwcConnection {
        for _ in 0 ..< 50 {
            if let connection = manager.state.nwc.connections.last(where: { !existingIds.contains($0.id) }) {
                return connection
            }
            try await Task.sleep(nanoseconds: 100_000_000)
        }
        throw NwaApprovalError.connectionTimedOut
    }
}

private struct NwaPolicyRow: View {
    let icon: String
    let title: String
    let value: String

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: icon)
                .font(.caption.bold())
                .foregroundStyle(mutedText)
                .frame(width: 18)
            VStack(alignment: .leading, spacing: 3) {
                Text(title)
                    .font(.caption.bold())
                    .foregroundStyle(mutedText)
                Text(value)
                    .font(.subheadline)
                    .foregroundStyle(primaryText)
                    .textSelection(.enabled)
            }
            Spacer(minLength: 0)
        }
    }
}

private enum NwaApprovalError: LocalizedError {
    case connectionTimedOut
    case invalidCallback

    var errorDescription: String? {
        switch self {
        case .connectionTimedOut:
            return "Timed out while creating the NWC connection."
        case .invalidCallback:
            return "The requesting app callback URL is invalid."
        }
    }
}

private enum NwaApprovalField: Hashable {
    case budget
}
