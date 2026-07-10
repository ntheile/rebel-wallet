import SwiftUI
import UIKit

struct NwcConnectionsView: View {
    @Bindable var manager: AppManager
    @Environment(\.walletAccent) private var walletAccent
    @State private var copiedConnectionId: String?
    @State private var deleteConnection: NwcConnection?

    private var connections: [NwcConnection] {
        manager.state.nwc.connections
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                createLink
                connectionsSection
                NwcWakeDebugCard(manager: manager)
            }
            .padding(16)
        }
        .navigationTitle("NWC")
        .background(pageBackground)
        .foregroundStyle(primaryText)
        .alert("Delete NWC string?", isPresented: Binding(
            get: { deleteConnection != nil },
            set: { if !$0 { deleteConnection = nil } }
        )) {
            Button("Delete", role: .destructive) {
                if let deleteConnection {
                    manager.unregisterNwcWakeConnection(deleteConnection)
                    manager.dispatch(.deleteNwcConnection(id: deleteConnection.id))
                }
                deleteConnection = nil
            }
            Button("Cancel", role: .cancel) {
                deleteConnection = nil
            }
        }
    }

    private var createLink: some View {
        NavigationLink {
            NwcCreateConnectionView(manager: manager)
        } label: {
            NwcCreateHeroButton(connectionCount: connections.count)
        }
        .buttonStyle(.plain)
        .accessibilityLabel("Create NWC")
    }

    private var connectionsSection: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text("Saved Strings")
                    .font(.caption.bold())
                    .foregroundStyle(mutedText)
                Spacer()
                Text("\(connections.count)")
                    .font(.caption.bold())
                    .foregroundStyle(connections.isEmpty ? mutedText : rebelGreen)
            }
            .padding(.horizontal, 4)

            if connections.isEmpty {
                VStack(alignment: .leading, spacing: 10) {
                    Image(systemName: "link.badge.plus")
                        .font(.title2)
                        .foregroundStyle(walletAccent)
                    Text("No NWC strings")
                        .font(.headline)
                    Text("Create one to authorize a Nostr Wallet Connect client.")
                        .font(.caption)
                        .foregroundStyle(mutedText)
                }
                .padding(16)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(surfaceBackground, in: RoundedRectangle(cornerRadius: 8))
                .overlay(RoundedRectangle(cornerRadius: 8).stroke(borderColor))
            } else {
                ForEach(connections, id: \.id) { connection in
                    NwcConnectionCard(
                        connection: connection,
                        uri: nwcUri(connection),
                        copied: copiedConnectionId == connection.id,
                        copy: { copy(connection) },
                        delete: { deleteConnection = connection }
                    )
                }
            }
        }
    }

    private func copy(_ connection: NwcConnection) {
        UIPasteboard.general.string = nwcUri(connection)
        copiedConnectionId = connection.id
        manager.requestHaptic(.impactLight)
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) {
            if copiedConnectionId == connection.id {
                copiedConnectionId = nil
            }
        }
    }

    private func nwcUri(_ connection: NwcConnection) -> String {
        NwcWakeRegistrationService.uriWithConnectionMetadata(
            connection.uri,
            lud16: manager.state.lightningAddress.address
        )
    }
}

private struct NwcCreateConnectionView: View {
    @Bindable var manager: AppManager
    @Environment(\.walletAccent) private var walletAccent
    @Environment(\.dismiss) private var dismiss
    @FocusState private var focusedField: NwcCreateField?
    @State private var name = ""
    @State private var relay = "wss://relay.getalby.com\nwss://relay2.getalby.com"
    @State private var budgetText = "10,000"
    @State private var budgetInterval: NwcBudgetInterval = .daily
    @State private var permissionPreset: NwcPermissionPreset = .fullAccess
    @State private var selectedPermissions = Set<NwcPermission>(NwcPermissionPreset.fullAccess.permissions)
    @State private var pendingCreatedConnectionExistingIds: Set<String>?
    @State private var customRelayDraft: NwcCustomRelayDraft?

    private var connections: [NwcConnection] {
        manager.state.nwc.connections
    }

    private var parsedBudget: UInt64? {
        let cleaned = budgetText.filter(\.isNumber)
        if cleaned.isEmpty {
            return 0
        }
        return UInt64(cleaned)
    }

    private var canCreate: Bool {
        parsedBudget != nil
            && !nwcRelayURLs(relay).isEmpty
    }

    private var permissionsForCreate: [NwcPermission] {
        switch permissionPreset {
        case .fullAccess, .readOnly:
            return permissionPreset.permissions
        case .custom:
            return selectedPermissions.sortedForDisplay
        }
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                NwcConnectionVisualization()
                    .frame(maxWidth: .infinity)

                SettingsCard(title: "New NWC String") {
                    VStack(alignment: .leading, spacing: 14) {
                        TextField("Name", text: $name)
                            .textInputAutocapitalization(.words)
                            .focused($focusedField, equals: .name)
                            .profileField()

                        VStack(alignment: .leading, spacing: 10) {
                            Text("Relay")
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
                                ForEach([5_000, 10_000, 500_000], id: \.self) { amount in
                                    Button {
                                        budgetText = formatBudgetInput("\(amount)")
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

                        permissionsSection

                        Button {
                            createConnection()
                        } label: {
                            Label("Create NWC", systemImage: "plus.circle.fill")
                                .frame(maxWidth: .infinity)
                        }
                        .buttonStyle(PrimaryButtonStyle(color: walletAccent))
                        .disabled(!canCreate)
                    }
                    .padding(14)
                }
            }
            .padding(16)
        }
        .contentShape(Rectangle())
        .onTapGesture {
            focusedField = nil
        }
        .scrollDismissesKeyboard(.interactively)
        .navigationTitle("Create NWC")
        .background(pageBackground)
        .foregroundStyle(primaryText)
        .onAppear {
            relay = manager.state.nwc.defaultRelay
        }
        .onChange(of: manager.state.nwc.connections) { _, newConnections in
            copyPendingCreatedConnection(from: newConnections)
        }
        .sheet(item: $customRelayDraft) { draft in
            NwcCustomRelaySheet(relay: $relay, initialRelay: draft.url)
                .presentationDetents([.height(260), .medium])
        }
    }

    private var permissionsSection: some View {
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

    private func createConnection() {
        guard let parsedBudget else { return }
        let existingIds = Set(connections.map(\.id))
        pendingCreatedConnectionExistingIds = existingIds
        manager.dispatch(.createNwcConnection(
            name: name,
            relay: relay,
            budgetSat: parsedBudget,
            budgetInterval: budgetInterval,
            permissions: permissionsForCreate
        ))
        DispatchQueue.main.asyncAfter(deadline: .now() + 2) {
            if pendingCreatedConnectionExistingIds == existingIds {
                pendingCreatedConnectionExistingIds = nil
            }
        }
    }

    private func copyPendingCreatedConnection(from updatedConnections: [NwcConnection]) {
        guard let existingIds = pendingCreatedConnectionExistingIds else { return }
        guard let newest = updatedConnections.last(where: { !existingIds.contains($0.id) }) else {
            if updatedConnections.count <= existingIds.count {
                pendingCreatedConnectionExistingIds = nil
            }
            return
        }

        pendingCreatedConnectionExistingIds = nil
        UIPasteboard.general.string = NwcWakeRegistrationService.uriWithConnectionMetadata(
            newest.uri,
            lud16: manager.state.lightningAddress.address
        )
        manager.requestHaptic(.impactLight)
        dismiss()
    }

    private func selectPermissionPreset(_ preset: NwcPermissionPreset) {
        permissionPreset = preset
        if preset != .custom {
            selectedPermissions = Set(preset.permissions)
        }
        manager.requestHaptic(.selection)
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
}

struct NwaWalletAuthApprovalView: View {
    @Bindable var manager: AppManager
    @Environment(\.dismiss) private var dismiss
    @Environment(\.walletAccent) private var walletAccent
    @FocusState private var focusedField: NwcCreateField?
    let request: NwaWalletCreatedRequest
    @State private var relay = ""
    @State private var budgetText = ""
    @State private var budgetInterval: NwcBudgetInterval = .daily
    @State private var permissionPreset: NwcPermissionPreset = .fullAccess
    @State private var selectedPermissions = Set<NwcPermission>()
    @State private var initialized = false
    @State private var editing = false
    @State private var customRelayDraft: NwcCustomRelayDraft?
    @State private var approving = false
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
                                    if !request.appId.isEmpty {
                                        Text(request.appId)
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
                                        ForEach([5_000, 10_000, 500_000], id: \.self) { amount in
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
                                Text("This mobile-to-mobile flow returns a full NWC URI to the requesting app. Only approve apps you trust.")
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
                                Task { await approve() }
                            } label: {
                                HStack {
                                    if approving {
                                        ProgressView()
                                            .tint(.white)
                                    }
                                    Text(approving ? "Connecting..." : "Connect")
                                }
                                .frame(maxWidth: .infinity)
                            }
                            .buttonStyle(PrimaryButtonStyle(color: walletAccent))
                            .disabled(approving || !canApprove)

                            Button {
                                cancel()
                            } label: {
                                Text("Cancel")
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
        var createdConnection: NwcConnection?
        let effectivePermissions = permissionsForCreate
        NwcWakeInbox.appendDebug(
            source: "App",
            message: "NWA approval settings callback=\(request.callbackTargetDescription) budget_sat=\(parsedBudget) interval=\(budgetInterval.title.lowercased()) relays=\(relays.joined(separator: ",")) permissions=\(effectivePermissions.map(\.methodName).joined(separator: ","))"
        )
        manager.dispatch(.createNwcConnection(
            name: request.displayName,
            relay: relayStorage,
            budgetSat: parsedBudget,
            budgetInterval: budgetInterval,
            permissions: effectivePermissions
        ))

        do {
            let connection = try await waitForCreatedConnection(existingIds: existingIds)
            createdConnection = connection
            try await manager.registerNwcWakeConnectionForNwa(connection)
            let nwcUri = NwcWakeRegistrationService.uriWithConnectionMetadata(
                connection.uri,
                lud16: manager.state.lightningAddress.address
            )
            guard let callback = request.approvedCallback(nwcUri: nwcUri) else {
                throw NwaApprovalError.invalidCallback
            }
            guard await manager.openVerifiedNwaCallback(callback) else {
                throw NwaApprovalError.callbackDeliveryFailed
            }
            await MainActor.run {
                manager.dismissNwaWalletRequest(request)
                dismiss()
            }
        } catch {
            if let createdConnection {
                await manager.rollbackNwaConnection(createdConnection)
            }
            await MainActor.run {
                approving = false
                errorMessage = error.localizedDescription
            }
        }
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
        Task {
            if let callback = request.cancelledCallback() {
                _ = await manager.openVerifiedNwaCallback(callback)
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
    case callbackDeliveryFailed

    var errorDescription: String? {
        switch self {
        case .connectionTimedOut:
            return "Timed out while creating the NWC connection."
        case .invalidCallback:
            return "The requesting app callback URL is invalid."
        case .callbackDeliveryFailed:
            return "The verified requesting app could not open the callback. The NWC connection was revoked."
        }
    }
}

private enum NwcCreateField: Hashable {
    case name
    case budget
}

private struct NwcCustomRelayDraft: Identifiable {
    let id = UUID()
    let url: String
}

private struct NwcRelayPreset: Identifiable, Hashable {
    let id: String
    let title: String
    let caption: String
    let urls: [String]
    let icon: String

    static let all: [NwcRelayPreset] = [
        NwcRelayPreset(
            id: "alby",
            title: "Alby NWC",
            caption: "Dedicated NWC relay with fallback",
            urls: [
                "wss://relay.getalby.com",
                "wss://relay2.getalby.com",
            ],
            icon: "bolt.fill"
        ),
        NwcRelayPreset(
            id: "primal",
            title: "Primal",
            caption: "Public Nostr relay",
            urls: ["wss://relay.primal.net"],
            icon: "antenna.radiowaves.left.and.right"
        ),
        NwcRelayPreset(
            id: "noslol",
            title: "nos.lol",
            caption: "Public Nostr relay",
            urls: ["wss://nos.lol"],
            icon: "antenna.radiowaves.left.and.right"
        ),
        NwcRelayPreset(
            id: "nostrband",
            title: "Nostr.band",
            caption: "Public Nostr relay",
            urls: ["wss://relay.nostr.band"],
            icon: "antenna.radiowaves.left.and.right"
        ),
    ]

    static func matching(_ url: String) -> NwcRelayPreset? {
        let normalized = nwcRelayURLs(url)
        return all.first { $0.urls == normalized }
    }
}

private struct NwcRelayPresetMenu: View {
    let selection: NwcRelayPreset?
    let relay: String
    let select: (NwcRelayPreset?) -> Void
    let editCustom: () -> Void

    var body: some View {
        Menu {
            ForEach(NwcRelayPreset.all) { preset in
                Button {
                    select(preset)
                } label: {
                    Label(preset.title, systemImage: preset.icon)
                }
            }

            Button {
                editCustom()
            } label: {
                Label("Custom", systemImage: "link")
            }
        } label: {
            NwcRelayPresetLabel(selection: selection, relay: relay)
        }
        .buttonStyle(.plain)
        .accessibilityLabel("NWC relay preset")
    }
}

private struct NwcCustomRelaySheet: View {
    @Binding var relay: String
    @Environment(\.dismiss) private var dismiss
    @FocusState private var focused: Bool
    @State private var primaryRelay: String
    @State private var fallbackRelay: String

    private var cleanedRelays: [String] {
        nwcRelayURLs([primaryRelay, fallbackRelay].joined(separator: "\n"))
    }

    private var canSave: Bool {
        !cleanedRelays.isEmpty
    }

    init(relay: Binding<String>, initialRelay: String) {
        _relay = relay
        let relays = nwcRelayURLs(initialRelay)
        _primaryRelay = State(initialValue: relays.first ?? "")
        _fallbackRelay = State(initialValue: relays.dropFirst().first ?? "")
    }

    var body: some View {
        NavigationStack {
            VStack(alignment: .leading, spacing: 16) {
                Text("Custom Relay")
                    .font(.headline)
                    .foregroundStyle(primaryText)

                TextField("wss://relay.example.com", text: $primaryRelay)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .keyboardType(.URL)
                    .focused($focused)
                    .profileField()

                TextField("Optional fallback relay", text: $fallbackRelay)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .keyboardType(.URL)
                    .profileField()

                Text("Use websocket relay URLs that support NWC events.")
                    .font(.caption)
                    .foregroundStyle(mutedText)

                Spacer(minLength: 0)
            }
            .padding(16)
            .background(pageBackground)
            .foregroundStyle(primaryText)
            .navigationTitle("Relay")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") {
                        dismiss()
                    }
                }

                ToolbarItem(placement: .confirmationAction) {
                    Button("Save") {
                        relay = encodeNwcRelayURLs(cleanedRelays)
                        dismiss()
                    }
                    .disabled(!canSave)
                }
            }
            .onAppear {
                focused = true
            }
        }
    }
}

private struct NwcRelayPresetLabel: View {
    let selection: NwcRelayPreset?
    let relay: String

    private var title: String {
        selection?.title ?? "Custom"
    }

    private var caption: String {
        if let selection {
            return selection.urls.joined(separator: " + ")
        }

        let relays = nwcRelayURLs(relay)
        return relays.isEmpty ? "Enter relay URL" : relays.joined(separator: " + ")
    }

    private var icon: String {
        selection?.icon ?? "link"
    }

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: icon)
                .font(.system(size: 19, weight: .medium))
                .foregroundStyle(mutedText)
                .frame(width: 28, height: 28)

            VStack(alignment: .leading, spacing: 4) {
                Text(title)
                    .font(.subheadline.bold())
                    .foregroundStyle(primaryText)

                Text(caption)
                    .font(.caption)
                    .foregroundStyle(mutedText)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }

            Spacer()

            Image(systemName: "chevron.up.chevron.down")
                .font(.caption.weight(.semibold))
                .foregroundStyle(mutedText)
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 14)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(raisedSurface, in: RoundedRectangle(cornerRadius: 8))
        .overlay(RoundedRectangle(cornerRadius: 8).stroke(borderColor))
        .contentShape(RoundedRectangle(cornerRadius: 8))
    }
}

private struct NwcCreateHeroButton: View {
    let connectionCount: Int
    @Environment(\.walletAccent) private var walletAccent

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            NwcConnectionVisualization()

            Label("Create NWC", systemImage: "plus.circle.fill")
                .font(.headline)
                .frame(maxWidth: .infinity)
                .padding(.vertical, 12)
                .foregroundStyle(primaryText)
                .background(walletAccent, in: RoundedRectangle(cornerRadius: 8))
        }
        .padding(16)
        .background(surfaceBackground, in: RoundedRectangle(cornerRadius: 8))
        .overlay(RoundedRectangle(cornerRadius: 8).stroke(borderColor))
    }
}

private struct NwcConnectionVisualization: View {
    var body: some View {
        HStack(spacing: 0) {
            VStack(spacing: 10) {
                RebelMark(size: 68)
                Text("Rebel Wallet")
                    .font(.caption.bold())
                    .foregroundStyle(primaryText)
                    .lineLimit(1)
                    .minimumScaleFactor(0.8)
            }
            .frame(maxWidth: .infinity)

            ZStack {
                Rectangle()
                    .fill(
                        LinearGradient(
                            colors: [.clear, mutedText.opacity(0.65), .clear],
                            startPoint: .leading,
                            endPoint: .trailing
                        )
                    )
                    .frame(height: 4)

                Image(systemName: "cable.connector.horizontal")
                    .font(.system(size: 34, weight: .medium))
                    .foregroundStyle(mutedText)
                    .padding(.horizontal, 8)
                    .background(Color.black, in: Capsule())
            }
            .frame(maxWidth: .infinity)

            VStack(spacing: 10) {
                ZStack {
                    RoundedRectangle(cornerRadius: 16)
                        .fill(Color.white.opacity(0.92))
                    Image(systemName: "square.grid.2x2")
                        .font(.system(size: 38, weight: .semibold))
                        .foregroundStyle(Color.black.opacity(0.55))
                }
                .frame(width: 68, height: 68)

                Text("External App")
                    .font(.caption.bold())
                    .foregroundStyle(primaryText)
                    .lineLimit(1)
                    .minimumScaleFactor(0.8)
            }
            .frame(maxWidth: .infinity)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 18)
        .frame(minHeight: 138)
        .background(Color.black, in: RoundedRectangle(cornerRadius: 8))
    }
}

private enum NwcPermissionPreset: String, CaseIterable, Identifiable {
    case fullAccess
    case readOnly
    case custom

    var id: String { rawValue }

    var title: String {
        switch self {
        case .fullAccess: return "Full Access"
        case .readOnly: return "Read Only"
        case .custom: return "Custom"
        }
    }

    var caption: String {
        switch self {
        case .fullAccess: return "Send and receive payments"
        case .readOnly: return "Receive payments and view history"
        case .custom: return "Define specific permissions"
        }
    }

    var icon: String {
        switch self {
        case .fullAccess: return "arrow.up.arrow.down"
        case .readOnly: return "arrow.down"
        case .custom: return "square.and.pencil"
        }
    }

    var color: Color {
        switch self {
        case .fullAccess: return rebelGreen
        case .readOnly: return rebelBlue
        case .custom: return mutedText
        }
    }

    var permissions: [NwcPermission] {
        switch self {
        case .fullAccess:
            return NwcPermission.createOptions
        case .readOnly:
            return [
                .getInfo,
                .getBalance,
                .makeInvoice,
                .lookupInvoice,
                .listTransactions
            ]
        case .custom:
            return []
        }
    }

    static func matching(_ permissions: [NwcPermission]) -> NwcPermissionPreset {
        let selected = Set(permissions)
        if selected == Set(NwcPermissionPreset.fullAccess.permissions) {
            return .fullAccess
        }
        if selected == Set(NwcPermissionPreset.readOnly.permissions) {
            return .readOnly
        }
        return .custom
    }
}

private struct NwcPermissionPresetMenu: View {
    let selection: NwcPermissionPreset
    let select: (NwcPermissionPreset) -> Void

    var body: some View {
        Menu {
            ForEach(NwcPermissionPreset.allCases) { preset in
                Button {
                    select(preset)
                } label: {
                    Label(preset.title, systemImage: preset.icon)
                }
            }
        } label: {
            NwcPermissionPresetLabel(preset: selection)
        }
        .buttonStyle(.plain)
        .accessibilityLabel("NWC permission preset")
    }
}

private struct NwcPermissionPresetLabel: View {
    let preset: NwcPermissionPreset

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: preset.icon)
                .font(.system(size: 19, weight: .medium))
                .foregroundStyle(mutedText)
                .frame(width: 28, height: 28)

            VStack(alignment: .leading, spacing: 4) {
                Text(preset.title)
                    .font(.subheadline.bold())
                    .foregroundStyle(primaryText)

                Text(preset.caption)
                    .font(.caption)
                    .foregroundStyle(mutedText)
            }

            Spacer()

            Image(systemName: "chevron.up.chevron.down")
                .font(.caption.weight(.semibold))
                .foregroundStyle(mutedText)
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 14)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(raisedSurface, in: RoundedRectangle(cornerRadius: 8))
        .overlay(RoundedRectangle(cornerRadius: 8).stroke(borderColor))
        .contentShape(RoundedRectangle(cornerRadius: 8))
    }
}

private struct NwcPermissionToggleRow: View {
    let icon: String
    let title: String
    let caption: String
    let color: Color
    @Binding var isOn: Bool

    var body: some View {
        Toggle(isOn: $isOn) {
            HStack(spacing: 12) {
                ZStack {
                    RoundedRectangle(cornerRadius: 8)
                        .fill(color.opacity(0.18))
                    Image(systemName: icon)
                        .font(.system(size: 15, weight: .semibold))
                        .foregroundStyle(color)
                }
                .frame(width: 36, height: 36)

                VStack(alignment: .leading, spacing: 3) {
                    Text(title)
                        .font(.subheadline.bold())
                    Text(caption)
                        .font(.caption)
                        .foregroundStyle(mutedText)
                }
            }
        }
        .tint(color)
        .padding(12)
        .background(raisedSurface, in: RoundedRectangle(cornerRadius: 8))
        .overlay(RoundedRectangle(cornerRadius: 8).stroke(borderColor))
    }
}

private struct NwcConnectionCard: View {
    let connection: NwcConnection
    let uri: String
    let copied: Bool
    let copy: () -> Void
    let delete: () -> Void

    private var lastUsedText: String {
        guard let lastUsedAt = connection.lastUsedAt else {
            return "Never used"
        }
        return "Last used \(shortDate(lastUsedAt))"
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(alignment: .top, spacing: 12) {
                ZStack {
                    RoundedRectangle(cornerRadius: 8)
                        .fill(rebelBlue.opacity(0.2))
                    Image(systemName: "link")
                        .font(.headline)
                        .foregroundStyle(rebelBlue)
                }
                .frame(width: 42, height: 42)

                VStack(alignment: .leading, spacing: 4) {
                    Text(connection.name)
                        .font(.headline)
                    Text(nwcRelayURLs(connection.relay).joined(separator: " + "))
                        .font(.caption)
                        .foregroundStyle(mutedText)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }

                Spacer()

                Button(action: delete) {
                    Image(systemName: "trash")
                        .font(.headline)
                        .foregroundStyle(rebelRed)
                        .frame(width: 36, height: 36)
                }
                .buttonStyle(.plain)
                .accessibilityLabel("Delete NWC string")
            }

            HStack(spacing: 8) {
                NwcPolicyPill(text: connection.budgetDisplay, color: primaryText)
                NwcPolicyPill(text: connection.budgetIntervalDisplay, color: primaryText)
                if let permissionPreset = connection.permissionPresetForDisplay {
                    NwcPolicyPill(text: permissionPreset.title, color: permissionPreset.color)
                }
            }

            if connection.permissionPresetForDisplay == nil {
                LazyVGrid(columns: [GridItem(.adaptive(minimum: 108), spacing: 8)], alignment: .leading, spacing: 8) {
                    ForEach(connection.enabledPermissionsForDisplay, id: \.self) { permission in
                        NwcPolicyPill(text: permission.shortTitle, color: permission.color(walletAccent: rebelBlue))
                    }
                }
            }

            VStack(alignment: .leading, spacing: 6) {
                Text(truncateMiddle(uri, maxLength: 80, prefixCount: 28))
                    .font(.system(.caption, design: .monospaced))
                    .foregroundStyle(primaryText)
                    .textSelection(.enabled)
                    .lineLimit(3)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .padding(12)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(raisedSurface, in: RoundedRectangle(cornerRadius: 8))
            .overlay(RoundedRectangle(cornerRadius: 8).stroke(borderColor))

            HStack(spacing: 10) {
                Button(action: copy) {
                    Label(copied ? "Copied" : "Copy", systemImage: copied ? "checkmark" : "doc.on.doc")
                        .frame(maxWidth: .infinity)
                }
                .buttonStyle(SecondaryButtonStyle())

                ShareLink(item: uri) {
                    Label("Share", systemImage: "square.and.arrow.up")
                        .frame(maxWidth: .infinity)
                }
                .buttonStyle(SecondaryButtonStyle())
            }

            HStack(spacing: 8) {
                Text("Created \(shortDate(connection.createdAt))")
                Text(lastUsedText)
            }
            .font(.caption2)
            .foregroundStyle(mutedText)
        }
        .padding(16)
        .background(surfaceBackground, in: RoundedRectangle(cornerRadius: 8))
        .overlay(RoundedRectangle(cornerRadius: 8).stroke(borderColor))
    }
}

private struct NwcPolicyPill: View {
    let text: String
    let color: Color

    var body: some View {
        Text(text)
            .font(.caption.bold())
            .lineLimit(1)
            .minimumScaleFactor(0.8)
            .padding(.horizontal, 10)
            .padding(.vertical, 6)
            .foregroundStyle(color)
            .background(color.opacity(0.14), in: Capsule())
    }
}

private extension NwcConnection {
    var permissionPresetForDisplay: NwcPermissionPreset? {
        let enabled = Set(enabledPermissionsForDisplay)
        if enabled == Set(NwcPermissionPreset.fullAccess.permissions) {
            return .fullAccess
        }
        if enabled == Set(NwcPermissionPreset.readOnly.permissions) {
            return .readOnly
        }
        return nil
    }

    var enabledPermissionsForDisplay: [NwcPermission] {
        if permissionsConfigured {
            return permissions.sortedForDisplay
        }

        var legacy: [NwcPermission] = [.getInfo]
        if allowGetBalance {
            legacy.append(.getBalance)
        }
        if allowPayInvoice {
            legacy.append(.payInvoice)
        }
        return legacy.sortedForDisplay
    }
}

private extension Set where Element == NwcPermission {
    var sortedForDisplay: [NwcPermission] {
        NwcPermission.createOptions.filter { contains($0) }
    }
}

private extension Array where Element == NwcPermission {
    var sortedForDisplay: [NwcPermission] {
        NwcPermission.createOptions.filter { contains($0) }
    }
}

private extension NwcPermission {
    static var createOptions: [NwcPermission] {
        [
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

    var title: String {
        switch self {
        case .getInfo: return "Get info"
        case .getBalance: return "Get balance"
        case .payInvoice: return "Pay invoices"
        case .payKeysend: return "Pay keysend"
        case .makeInvoice: return "Make invoices"
        case .lookupInvoice: return "Lookup invoices"
        case .listTransactions: return "List transactions"
        case .makeHoldInvoice: return "Make hold invoices"
        case .cancelHoldInvoice: return "Cancel hold invoices"
        case .settleHoldInvoice: return "Settle hold invoices"
        }
    }

    var shortTitle: String {
        switch self {
        case .getInfo: return "Info"
        case .getBalance: return "Balance"
        case .payInvoice: return "Pay"
        case .payKeysend: return "Keysend"
        case .makeInvoice: return "Invoice"
        case .lookupInvoice: return "Lookup"
        case .listTransactions: return "History"
        case .makeHoldInvoice: return "Hold"
        case .cancelHoldInvoice: return "Cancel hold"
        case .settleHoldInvoice: return "Settle hold"
        }
    }

    var methodName: String {
        switch self {
        case .getInfo: return "get_info"
        case .getBalance: return "get_balance"
        case .payInvoice: return "pay_invoice"
        case .payKeysend: return "pay_keysend"
        case .makeInvoice: return "make_invoice"
        case .lookupInvoice: return "lookup_invoice"
        case .listTransactions: return "list_transactions"
        case .makeHoldInvoice: return "make_hold_invoice"
        case .cancelHoldInvoice: return "cancel_hold_invoice"
        case .settleHoldInvoice: return "settle_hold_invoice"
        }
    }

    var icon: String {
        switch self {
        case .getInfo: return "info.circle"
        case .getBalance: return "eye"
        case .payInvoice: return "bolt.fill"
        case .payKeysend: return "paperplane.fill"
        case .makeInvoice: return "plus.app"
        case .lookupInvoice: return "magnifyingglass"
        case .listTransactions: return "list.bullet.rectangle"
        case .makeHoldInvoice: return "lock.doc"
        case .cancelHoldInvoice: return "xmark.circle"
        case .settleHoldInvoice: return "checkmark.seal"
        }
    }

    func color(walletAccent: Color) -> Color {
        switch self {
        case .getInfo: return rebelBlue
        case .getBalance: return rebelGreen
        case .payInvoice: return walletAccent
        case .payKeysend: return rebelRed
        case .makeInvoice: return Color.cyan
        case .lookupInvoice: return Color.orange
        case .listTransactions: return Color.indigo
        case .makeHoldInvoice: return Color.yellow
        case .cancelHoldInvoice: return mutedText
        case .settleHoldInvoice: return Color.mint
        }
    }
}

private extension NwcBudgetInterval {
    static var createOptions: [NwcBudgetInterval] {
        [.hourly, .daily, .weekly, .monthly]
    }

    var title: String {
        switch self {
        case .hourly: return "Hourly"
        case .daily: return "Daily"
        case .weekly: return "Weekly"
        case .monthly: return "Monthly"
        }
    }
}

private func compactSats(_ amount: Int) -> String {
    if amount >= 1_000 {
        return "\(amount / 1_000)k"
    }
    return "\(amount)"
}

private func nwcRelayURLs(_ value: String) -> [String] {
    var seen = Set<String>()
    return value
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

private func encodeNwcRelayURLs(_ relays: [String]) -> String {
    nwcRelayURLs(relays.joined(separator: "\n")).joined(separator: "\n")
}

private func shortDate(_ unix: UInt64) -> String {
    let date = Date(timeIntervalSince1970: TimeInterval(unix))
    return date.formatted(date: .abbreviated, time: .shortened)
}
