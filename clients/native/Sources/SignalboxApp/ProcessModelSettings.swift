import Combine
import Foundation
import SwiftUI

@MainActor
final class ProcessModelSettingsViewModel: ObservableObject {
  @Published private(set) var defaults: SignalboxSessionDefaultsRead?
  @Published private(set) var catalog: [SignalboxModelCapabilityItem] = []
  @Published private(set) var aliases: [SignalboxModelAliasSummary] = []
  @Published var selection: SignalboxModelSelection
  @Published var sessionOverlay: SignalboxModelSettingsOverlay = .inheritAll
  @Published private(set) var isLoading = false
  @Published private(set) var isSaving = false
  @Published var errorMessage: String?
  private var unresolvedReplacement: SignalboxPreparedDefaultsReplacement?
  private let sessionID: SignalboxCanonicalUUID

  init(session: SignalboxProcessSession) {
    sessionID = session.id
    selection = session.modelSelection
  }

  var capabilities: SignalboxModelCapabilities? {
    .selected(selection, catalog: catalog, aliases: aliases)
  }

  var selectableModels: [SignalboxModelSelection] {
    var selections = catalog.map { SignalboxModelSelection.direct(selectionID: $0.selectionID) }
    selections += aliases.filter { alias in catalog.contains { $0.selectionID == alias.selectionID } }
      .map { .alias(aliasID: $0.aliasID) }
    if !selections.contains(selection) { selections.insert(selection, at: 0) }
    return selections
  }

  func supportedPerCallOverlay(_ overlay: SignalboxModelSettingsOverlay) -> SignalboxModelSettingsOverlay {
    guard let defaults else { return overlay }
    let capabilities = SignalboxModelCapabilities.selected(defaults.modelSelection,
      catalog: catalog, aliases: aliases)
    var supported = overlay
    if case .value(let level) = overlay.reasoningLevel,
      capabilities?.reasoningLevels.contains(level) != true {
      supported.reasoningLevel = .inherit
    }
    if case .value = overlay.fastMode, capabilities?.fastModeSupported != true {
      supported.fastMode = .inherit
    }
    if case .value(let tier) = overlay.serviceTier,
      capabilities?.serviceTiers.contains(tier) != true {
      supported.serviceTier = .inherit
    }
    return supported
  }

  func load(using service: any SignalboxProcessServiceProtocol) async {
    isLoading = true
    defer { isLoading = false }
    do {
      let defaults = try await service.readDefaults(sessionID: sessionID)
      let catalog = try await service.listModelCapabilities()
      let aliases = try await service.listModelAliases()
      self.defaults = defaults
      self.catalog = catalog
      self.aliases = aliases
      selection = defaults.modelSelection
      sessionOverlay = .inheritAll
      errorMessage = nil
    } catch { errorMessage = error.localizedDescription }
  }

  func save(using service: any SignalboxProcessServiceProtocol) async -> SignalboxSessionDefaultsRead? {
    guard let defaults, !isSaving, !isLoading else { return nil }
    isSaving = true
    defer { isSaving = false }
    do {
      let prepared: SignalboxPreparedDefaultsReplacement
      if let unresolvedReplacement,
        unresolvedReplacement.modelSelection == selection,
        unresolvedReplacement.modelSettings == sessionOverlay {
        prepared = unresolvedReplacement
      } else {
        prepared = try await service.prepareDefaultsReplacement(defaults: defaults,
          modelSelection: selection, modelSettings: sessionOverlay)
      }
      unresolvedReplacement = prepared
      let installed = try await service.replaceDefaults(prepared)
      unresolvedReplacement = nil
      self.defaults = installed
      sessionOverlay = .inheritAll
      errorMessage = nil
      return installed
    } catch {
      if let serviceError = error as? SignalboxProcessServiceError,
        !serviceError.retainsPreparedMutationIdentity {
        unresolvedReplacement = nil
      }
      errorMessage = error.localizedDescription
      if let serviceError = error as? SignalboxProcessServiceError,
        case .remote(code: .rejected, message: _,
          detail: .some(.defaultsVersionMismatch(let rejectedSessionID, expected: _, current: _))) = serviceError,
        rejectedSessionID == sessionID {
        do {
          let refreshed = try await service.readDefaults(sessionID: sessionID)
          if refreshed.modelSelection != defaults.modelSelection {
            selection = refreshed.modelSelection
            sessionOverlay = .inheritAll
          }
          self.defaults = refreshed
        } catch { errorMessage = error.localizedDescription }
      }
      return nil
    }
  }
}

struct ProcessModelSettingsScreen: View {
  @Environment(\.dismiss) private var dismiss
  @StateObject private var viewModel: ProcessModelSettingsViewModel
  @Binding var perCall: SignalboxModelSettingsOverlay
  let service: any SignalboxProcessServiceProtocol
  let adjustments: [SignalboxModelChangeAdjustment]
  let installed: (SignalboxSessionDefaultsRead) -> Void

  init(session: SignalboxProcessSession, perCall: Binding<SignalboxModelSettingsOverlay>,
    service: any SignalboxProcessServiceProtocol, adjustments: [SignalboxModelChangeAdjustment],
    installed: @escaping (SignalboxSessionDefaultsRead) -> Void) {
    _viewModel = StateObject(wrappedValue: ProcessModelSettingsViewModel(session: session))
    _perCall = perCall
    self.service = service
    self.adjustments = adjustments
    self.installed = installed
  }

  var body: some View {
    NavigationStack {
      Form {
        if let error = viewModel.errorMessage { Text(error).foregroundStyle(.red) }
        if viewModel.isLoading { ProgressView("Reading model settings") }
        if let defaults = viewModel.defaults {
          Section("Current session settings") {
            currentSettings(defaults.modelSettings)
          }
          Section("Session overrides") {
            Picker("Model", selection: $viewModel.selection) {
              ForEach(viewModel.selectableModels, id: \.self) { model in
                Text(model.settingsLabel).tag(model)
              }
            }
            ProcessSettingsOverlayFields(overlay: $viewModel.sessionOverlay,
              capabilities: viewModel.capabilities)
            Button("Save session settings") {
              Task {
                _ = await viewModel.save(using: service)
                if let defaults = viewModel.defaults { installed(defaults) }
                perCall = viewModel.supportedPerCallOverlay(perCall)
              }
            }
            .disabled(viewModel.isSaving)
            .accessibilityIdentifier("save-session-model-settings")
          }
          Section("Next input overrides") {
            ProcessSettingsOverlayFields(overlay: $perCall,
              capabilities: .selected(defaults.modelSelection,
                catalog: viewModel.catalog, aliases: viewModel.aliases))
            Text("These choices apply to the next input you send, including a stop or recovery successor.")
              .font(.caption).foregroundStyle(.secondary)
          }
          if !adjustments.isEmpty {
            Section("Latest recorded adjustments") {
              ForEach(Array(adjustments.enumerated()), id: \.offset) { _, adjustment in
                Text(adjustment.settingsLabel)
              }
            }
          }
        }
      }
      .disabled(viewModel.isSaving)
      .navigationTitle("Model settings")
      .toolbar {
        ToolbarItem(placement: .confirmationAction) {
          Button("Done") { dismiss() }.disabled(viewModel.isSaving)
        }
      }
      .task {
        await viewModel.load(using: service)
        if let defaults = viewModel.defaults { installed(defaults) }
        perCall = viewModel.supportedPerCallOverlay(perCall)
      }
      .onChange(of: viewModel.selection) { _, _ in viewModel.sessionOverlay = .inheritAll }
    }
    .interactiveDismissDisabled(viewModel.isSaving)
    .frame(minWidth: 360, minHeight: 480)
  }

  @ViewBuilder
  private func currentSettings(_ snapshot: SignalboxModelSettingsSnapshot) -> some View {
    LabeledContent("Reasoning", value: snapshot.effective.reasoningLevel?.rawValue ?? "Provider default")
    Text("\(snapshot.precedence.session.reasoningLevel.settingsLabel) · source: \(snapshot.reasoningSource?.rawValue ?? "provider default")")
      .font(.caption).foregroundStyle(.secondary)
    LabeledContent("Fast mode", value: snapshot.effective.fastMode.rawValue)
    Text("\(snapshot.precedence.session.fastMode.settingsLabel) · source: \(snapshot.fastModeSource?.rawValue ?? "provider default")")
      .font(.caption).foregroundStyle(.secondary)
    LabeledContent("Service tier", value: snapshot.effective.serviceTier?.settingsLabel ?? "Provider default")
    Text("\(snapshot.precedence.session.serviceTier.settingsLabel) · source: \(snapshot.serviceTierSource?.rawValue ?? "provider default")")
      .font(.caption).foregroundStyle(.secondary)
  }
}

struct ProcessSettingsOverlayFields: View {
  @Binding var overlay: SignalboxModelSettingsOverlay
  let capabilities: SignalboxModelCapabilities?

  var body: some View {
    Picker("Reasoning", selection: $overlay.reasoningLevel) {
      Text("Inherit").tag(SignalboxSettingOverlay<SignalboxReasoningLevel>.inherit)
      Text("Provider default").tag(SignalboxSettingOverlay<SignalboxReasoningLevel>.providerDefault)
      ForEach(capabilities?.reasoningLevels ?? [], id: \.self) { level in
        Text(level.rawValue).tag(SignalboxSettingOverlay.value(level))
      }
    }
    Picker("Fast mode", selection: $overlay.fastMode) {
      Text("Inherit").tag(SignalboxFastModeOverlay.inherit)
      if capabilities?.fastModeSupported == true {
        ForEach(SignalboxFastMode.allCases, id: \.self) { value in
          Text(value.rawValue).tag(SignalboxFastModeOverlay.value(value))
        }
      }
    }
    Picker("Service tier", selection: $overlay.serviceTier) {
      Text("Inherit").tag(SignalboxSettingOverlay<SignalboxServiceTier>.inherit)
      Text("Provider default").tag(SignalboxSettingOverlay<SignalboxServiceTier>.providerDefault)
      ForEach(capabilities?.serviceTiers ?? [], id: \.self) { tier in
        Text(tier.settingsLabel).tag(SignalboxSettingOverlay.value(tier))
      }
    }
  }
}

extension SignalboxModelSelection {
  var settingsLabel: String {
    switch self {
    case .direct(let id): "Direct \(id.rawValue)"
    case .alias(let id): "Alias \(id.rawValue)"
    }
  }
}

extension SignalboxServiceTier {
  var settingsLabel: String { "\(wireValue.provider): \(wireValue.value)" }
}

extension SignalboxSettingOverlay {
  var settingsLabel: String {
    switch self {
    case .inherit: "Inherited"
    case .providerDefault: "Explicit provider default"
    case .value: "Explicit value"
    }
  }
}

extension SignalboxFastModeOverlay {
  var settingsLabel: String {
    switch self {
    case .inherit: "Inherited"
    case .value: "Explicit value"
    }
  }
}

extension SignalboxModelChangeAdjustment {
  var settingsLabel: String {
    switch self {
    case .reasoningLevelClamped(let from, let to): "Reasoning adjusted from \(from.rawValue) to \(to.rawValue)"
    case .reasoningLevelCleared(let from): "Reasoning \(from.rawValue) cleared to provider default"
    case .fastModeDisabled: "Fast mode disabled"
    case .serviceTierCleared(let from): "Service tier \(from.settingsLabel) cleared to provider default"
    }
  }
}
