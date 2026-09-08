import Foundation
import XCTest

@testable import SignalboxNative

final class ModelSettingsTests: XCTestCase {
  @MainActor
  func testSettingsReadDoesNotCopyExplicitDefaultsIntoTheDraft() async throws {
    let requester = try SettingsRequester(reading: .value(.high))
    let service = SignalboxProcessService(requester: requester, policy: .nativeDefault)
    let viewModel = ProcessModelSettingsViewModel(session: try SettingsFixture.session())

    await viewModel.load(using: service)

    XCTAssertNil(viewModel.errorMessage)
    XCTAssertEqual(viewModel.defaults?.modelSettings.precedence.session.reasoningLevel, .value(.high))
    XCTAssertEqual(viewModel.sessionOverlay, .inheritAll)
    XCTAssertEqual(viewModel.capabilities?.reasoningLevels, [.low, .high])
  }

  @MainActor
  func testDefaultsReplacementCarriesOnlyTheChosenOverlay() async throws {
    let requester = try SettingsRequester(reading: .value(.high), replacement: .providerDefault)
    let service = SignalboxProcessService(requester: requester, policy: .nativeDefault)
    let viewModel = ProcessModelSettingsViewModel(session: try SettingsFixture.session())
    await viewModel.load(using: service)
    viewModel.sessionOverlay.reasoningLevel = .providerDefault

    let installed = await viewModel.save(using: service)

    XCTAssertNil(viewModel.errorMessage)
    XCTAssertEqual(installed?.modelSettings.precedence.session.reasoningLevel, .providerDefault)
    XCTAssertEqual(installed?.defaultsVersion.rawValue, 2)
    let lastRequest = await requester.lastRequest()
    let request = try XCTUnwrap(lastRequest)
    guard case .replaceSessionDefaults(_, let prior, let selection, let overlay) = request else {
      return XCTFail("Expected a defaults replacement.")
    }
    XCTAssertEqual(prior.defaultsVersion.rawValue, 1)
    XCTAssertEqual(selection, .direct(selectionID: SettingsFixture.selectionID))
    XCTAssertEqual(overlay, .init(reasoningLevel: .providerDefault, fastMode: .inherit, serviceTier: .inherit))
    let encoded = try SignalboxJSONCoding.encoder().encode(request)
    let fields = try SignalboxJSONCoding.decoder().decode([String: SignalboxJSONValue].self, from: encoded)
    XCTAssertEqual(fields["system_prompt"], .string(SettingsFixture.systemPrompt))
    XCTAssertEqual(fields["dangerous_tool_auto_approval"], .bool(true))
  }

  @MainActor
  func testCancelledSettingsSaveDiscardsTheOldServiceReceipt() async throws {
    let requester = try SettingsRequester(reading: .value(.high), replacement: .providerDefault,
      suspendsReplacement: true)
    let service = SignalboxProcessService(requester: requester, policy: .nativeDefault)
    let viewModel = ProcessModelSettingsViewModel(session: try SettingsFixture.session())
    await viewModel.load(using: service)
    viewModel.sessionOverlay.reasoningLevel = .providerDefault
    let save = Task { await viewModel.save(using: service) }
    await requester.waitUntilReplacementStarted()

    save.cancel()
    await requester.resumeReplacement()
    let installed = await save.value

    XCTAssertNil(installed)
    XCTAssertEqual(viewModel.defaults?.defaultsVersion.rawValue, 1)
    XCTAssertEqual(viewModel.defaults?.modelSettings.precedence.session.reasoningLevel, .value(.high))
    XCTAssertEqual(viewModel.sessionOverlay.reasoningLevel, .providerDefault)
    XCTAssertNil(viewModel.errorMessage)
    XCTAssertFalse(viewModel.isSaving)
  }

  @MainActor
  func testDefaultsVersionConflictRefreshesTheNextSaveAndResetsChangedModelChoices() async throws {
    for refreshedSelection in [SettingsFixture.selectionID, SettingsFixture.otherSelectionID] {
      let requester = try SettingsRequester(reading: .value(.high))
      let service = SignalboxProcessService(requester: requester, policy: .nativeDefault)
      let viewModel = ProcessModelSettingsViewModel(session: try SettingsFixture.session())
      await viewModel.load(using: service)
      viewModel.sessionOverlay.reasoningLevel = .providerDefault
      let modelChanged = refreshedSelection != SettingsFixture.selectionID
      let nextReasoning: SignalboxSettingOverlay<SignalboxReasoningLevel> = modelChanged ? .inherit : .providerDefault
      await requester.append(pages: [
        [try SettingsFixture.frame(["type": "error", "code": "rejected", "message": "Defaults changed.",
          "detail": ["type": "defaults_version_mismatch", "session_id": SettingsFixture.sessionID.rawValue,
            "expected": "1", "current": "2"]])],
        [try SettingsFixture.defaults(reasoning: .value(.low), version: "2", selectionID: refreshedSelection)],
        [try SettingsFixture.defaults(reasoning: modelChanged ? .value(.low) : nextReasoning, version: "3",
          type: "session_defaults_replaced", selectionID: refreshedSelection)],
      ])

      let rejected = await viewModel.save(using: service)

      XCTAssertNil(rejected)
      XCTAssertNotNil(viewModel.errorMessage)
      XCTAssertEqual(viewModel.defaults?.defaultsVersion.rawValue, 2)
      XCTAssertEqual(viewModel.selection, .direct(selectionID: refreshedSelection))
      XCTAssertEqual(viewModel.sessionOverlay.reasoningLevel, nextReasoning)
      XCTAssertFalse(viewModel.isSaving)

      let installed = await viewModel.save(using: service)

      XCTAssertNil(viewModel.errorMessage)
      XCTAssertEqual(installed?.defaultsVersion.rawValue, 3)
      let requests = await requester.openedRequests()
      let replacements = requests.compactMap { request -> (SignalboxCommandID, UInt64)? in
        guard case .replaceSessionDefaults(let commandID, let defaults, _, _) = request else { return nil }
        return (commandID, defaults.defaultsVersion.rawValue)
      }
      XCTAssertEqual(replacements.map { $0.1 }, [1, 2])
      XCTAssertNotEqual(replacements.first?.0, replacements.last?.0)
    }
  }

  func testDefaultsReplacementRejectsSettingsThatDoNotMatchTheRequest() async throws {
    guard case .sessionDefaults(let prior) = try SettingsFixture.defaults(
      reasoning: .value(.high), version: "1").message else { return XCTFail("Expected defaults.") }
    let invalidReceipts = [
      try SettingsFixture.defaults(reasoning: .value(.low), version: "2", type: "session_defaults_replaced"),
      try SettingsFixture.defaults(reasoning: .inherit, version: "2", type: "session_defaults_replaced"),
      try SettingsFixture.defaults(reasoning: .value(.high), version: "2", type: "session_defaults_replaced",
        fastMode: .value(.disabled)),
      try SettingsFixture.defaults(reasoning: .value(.high), version: "2", type: "session_defaults_replaced",
        serviceTier: .value(.openAI(.priority))),
    ]
    for receipt in invalidReceipts {
      let service = SignalboxProcessService(requester: SettingsRequester(pages: [[receipt]]), policy: .nativeDefault)
      let prepared = try await service.prepareDefaultsReplacement(defaults: prior,
        modelSelection: prior.modelSelection, modelSettings: .inheritAll)
      do {
        _ = try await service.replaceDefaults(prepared)
        XCTFail("A replacement receipt must preserve the requested settings and provenance.")
      } catch let error as SignalboxProcessServiceError {
        XCTAssertEqual(error, .unexpectedMessage("The defaults receipt did not match the replacement."))
      }
    }
  }

  func testDefaultsReplacementAllowsModelAdjustmentsOnlyForInheritedSettings() async throws {
    guard case .sessionDefaults(let prior) = try SettingsFixture.defaults(reasoning: .value(.high), version: "1",
      fastMode: .value(.enabled), serviceTier: .value(.openAI(.priority))).message else {
      return XCTFail("Expected defaults.")
    }
    let receipt = try SettingsFixture.defaults(reasoning: .value(.low), version: "2",
      type: "session_defaults_replaced", selectionID: SettingsFixture.otherSelectionID,
      fastMode: .value(.disabled), serviceTier: .providerDefault)
    let service = SignalboxProcessService(requester: SettingsRequester(pages: [[receipt], [receipt]]), policy: .nativeDefault)
    let inherited = try await service.prepareDefaultsReplacement(defaults: prior,
      modelSelection: .direct(selectionID: SettingsFixture.otherSelectionID), modelSettings: .inheritAll)

    let installed = try await service.replaceDefaults(inherited)

    XCTAssertEqual(installed.modelSettings.precedence.session,
      .init(reasoningLevel: .value(.low), fastMode: .value(.disabled), serviceTier: .providerDefault))
    let explicit = try await service.prepareDefaultsReplacement(defaults: prior,
      modelSelection: inherited.modelSelection, modelSettings: prior.modelSettings.precedence.session)
    do {
      _ = try await service.replaceDefaults(explicit)
      XCTFail("Model changes must not adjust explicit caller settings.")
    } catch let error as SignalboxProcessServiceError {
      XCTAssertEqual(error, .unexpectedMessage("The defaults receipt did not match the replacement."))
    }
  }

  @MainActor
  func testAliasRetargetClearsOnlyUnsupportedPerCallValues() async throws {
    let requester = SettingsRequester(pages: [])
    for target in [SettingsFixture.selectionID, SettingsFixture.otherSelectionID] {
      let retargeted = target == SettingsFixture.otherSelectionID
      await requester.append(pages: [
        [try SettingsFixture.defaults(reasoning: .inherit, version: "1",
          selectionID: target, aliasID: SettingsFixture.aliasID)],
        [try SettingsFixture.frame(["type": "model_capabilities_start"]),
          try SettingsFixture.frame(["type": "model_capability_item", "selection_id": target.rawValue,
            "capabilities": ["reasoning_levels": retargeted ? ["low"] : ["low", "high"],
              "fast_mode_supported": !retargeted,
              "service_tiers": [["provider": "open_ai", "value": "priority"]]]]),
          try SettingsFixture.frame(["type": "model_capabilities_end", "capability_count": "1"])],
        [try SettingsFixture.frame(["type": "model_aliases_start"]),
          try SettingsFixture.frame(["type": "model_alias_summary", "alias_id": SettingsFixture.aliasID.rawValue,
            "selection_id": target.rawValue]),
          try SettingsFixture.frame(["type": "model_aliases_end", "alias_count": "1"])],
      ])
    }
    let service = SignalboxProcessService(requester: requester, policy: .nativeDefault)
    let viewModel = ProcessModelSettingsViewModel(session: try SettingsFixture.session())
    let perCall = SignalboxModelSettingsOverlay(reasoningLevel: .value(.high),
      fastMode: .value(.enabled), serviceTier: .value(.openAI(.priority)))
    await viewModel.load(using: service)
    XCTAssertNil(viewModel.errorMessage)
    let originalSelection = viewModel.defaults?.modelSelection
    XCTAssertEqual(viewModel.supportedPerCallOverlay(perCall), perCall)

    await viewModel.load(using: service)

    XCTAssertNil(viewModel.errorMessage)
    XCTAssertEqual(viewModel.defaults?.modelSelection, originalSelection)
    XCTAssertEqual(viewModel.supportedPerCallOverlay(perCall),
      .init(reasoningLevel: .inherit, fastMode: .inherit, serviceTier: .value(.openAI(.priority))))
    XCTAssertEqual(viewModel.supportedPerCallOverlay(.init(reasoningLevel: .value(.low),
      fastMode: .inherit, serviceTier: .value(.openAI(.flex)))),
      .init(reasoningLevel: .value(.low), fastMode: .inherit, serviceTier: .inherit))
    let cleared = SignalboxModelSettingsOverlay(reasoningLevel: .providerDefault,
      fastMode: .inherit, serviceTier: .providerDefault)
    XCTAssertEqual(viewModel.supportedPerCallOverlay(cleared), cleared)
    XCTAssertEqual(viewModel.supportedPerCallOverlay(.inheritAll), .inheritAll)
    let disabled = SignalboxModelSettingsOverlay(reasoningLevel: .inherit,
      fastMode: .value(.disabled), serviceTier: .inherit)
    XCTAssertEqual(viewModel.supportedPerCallOverlay(disabled), disabled)
  }

  func testCapabilityCatalogRejectsAnIncompleteSequence() async throws {
    let requester = SettingsRequester(pages: [[try SettingsFixture.frame(["type": "model_capabilities_start"]) ]])
    let service = SignalboxProcessService(requester: requester, policy: .nativeDefault)
    do {
      _ = try await service.listModelCapabilities()
      XCTFail("An unterminated catalog must not supply settings choices.")
    } catch let error as SignalboxProcessServiceError {
      XCTAssertEqual(error, .unexpectedMessage("The model-capability sequence ended before its terminator."))
    }
  }

  func testCapabilityCatalogStopsAtTheProtocolEntryLimit() async throws {
    let tooMany = SignalboxProcessProtocol.maximumModelCapabilityCatalogEntries + 1
    let entries = try (1...tooMany).map { ordinal in
      try SettingsFixture.frame(["type": "model_capability_item",
        "selection_id": String(format: "00000000-0000-4000-8000-%012x", ordinal),
        "capabilities": ["reasoning_levels": [], "fast_mode_supported": false, "service_tiers": []]])
    }
    let requester = SettingsRequester(pages: [[try SettingsFixture.frame(["type": "model_capabilities_start"])] + entries])
    let service = SignalboxProcessService(requester: requester, policy: .nativeDefault)
    do {
      _ = try await service.listModelCapabilities()
      XCTFail("The catalog must stop retaining entries before waiting for a terminator.")
    } catch let error as SignalboxProcessServiceError {
      XCTAssertEqual(error, .invalidPage("The model-capability catalog exceeded the protocol limit."))
    }
  }

  func testPerCallSettingsReachEveryNativeInputCommand() async throws {
    let overlay = SignalboxModelSettingsOverlay(reasoningLevel: .value(.low),
      fastMode: .inherit, serviceTier: .providerDefault)
    let refusal = try SettingsFixture.frame(["type": "error", "code": "invalid_request",
      "message": "Fixture rejects input after recording its request."])
    let requester = SettingsRequester(pages: [[refusal], [refusal], [refusal]])
    let service = SignalboxProcessService(requester: requester, policy: .nativeDefault)
    let session = try SettingsFixture.session()
    let input = try await service.prepareInputSubmission(session: session, content: SettingsFixture.input,
      modelSettings: overlay)
    let reconciliation = try await service.prepareTurnReconciliation(session: session,
      activeTurnID: SettingsFixture.turnID, content: SettingsFixture.input, modelSettings: overlay)
    let stop = try await service.prepareTurnStop(session: session,
      activeTurnID: SettingsFixture.turnID, content: SettingsFixture.input, modelSettings: overlay)

    do { _ = try await service.submit(input); XCTFail("Expected fixture refusal.") }
    catch let error as SignalboxProcessServiceError {
      XCTAssertEqual(error, .remote(code: .invalidRequest,
        message: "Fixture rejects input after recording its request.", detail: nil))
    }
    do { _ = try await service.reconcileTurn(reconciliation); XCTFail("Expected fixture refusal.") }
    catch let error as SignalboxProcessServiceError {
      XCTAssertEqual(error, .remote(code: .invalidRequest,
        message: "Fixture rejects input after recording its request.", detail: nil))
    }
    do { _ = try await service.stopTurn(stop); XCTFail("Expected fixture refusal.") }
    catch let error as SignalboxProcessServiceError {
      XCTAssertEqual(error, .remote(code: .invalidRequest,
        message: "Fixture rejects input after recording its request.", detail: nil))
    }

    let requests = await requester.openedRequests()
    XCTAssertEqual(requests.count, 3)
    for request in requests {
      let encoded = try SignalboxJSONCoding.encoder().encode(request)
      let fields = try SignalboxJSONCoding.decoder().decode([String: SignalboxJSONValue].self, from: encoded)
      XCTAssertEqual(fields["model_settings"], .object([
        "reasoning_level": .object(["kind": .string("value"), "value": .string("low")]),
        "fast_mode": .object(["kind": .string("inherit")]),
        "service_tier": .object(["kind": .string("provider_default")]),
      ]))
    }
  }

  @MainActor
  func testReopenedDetailRestoresTheLatestTurnsRecordedAdjustments() throws {
    let session = try SettingsFixture.session()
    let olderTurn = try SettingsFixture.turn(position: 1, adjustedFromReasoning: .max)
    let latestTurn = try SettingsFixture.turn(position: 2, adjustedFromReasoning: .high)
    let reopened = ProcessSessionDetailViewModel(session: session) { nil }

    reopened.apply(.authoritativeSnapshot(.init(sessionID: session.id,
      cursor: .init(rawValue: 10), records: [.turn(olderTurn), .turn(latestTurn)])))

    XCTAssertNil(reopened.errorMessage)
    XCTAssertEqual(reopened.settingsAdjustments, [.reasoningLevelClamped(from: .high, to: .low)])
    XCTAssertEqual(reopened.settingsAdjustments.map(\.settingsLabel), ["Reasoning adjusted from high to low"])
  }

  @MainActor
  func testAuthoritativeSnapshotClearsAdjustmentsWhenTheLatestTurnHasNone() throws {
    let session = try SettingsFixture.session()
    let olderTurn = try SettingsFixture.turn(position: 1, adjustedFromReasoning: .high)
    let latestTurn = try SettingsFixture.turn(position: 2, adjustedFromReasoning: nil)
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }
    viewModel.apply(.event(.init(cursor: .init(rawValue: 1), sessionID: session.id,
      event: .turnModelSettingsResolved(adjustments: [.fastModeDisabled]))))

    viewModel.apply(.authoritativeSnapshot(.init(sessionID: session.id,
      cursor: .init(rawValue: 10), records: [.turn(olderTurn), .turn(latestTurn)])))

    XCTAssertNil(viewModel.errorMessage)
    XCTAssertEqual(latestTurn.settingsAdjustments, [])
    XCTAssertEqual(viewModel.settingsAdjustments, [])
  }

  @MainActor
  func testSettingsPresentationUsesOnlyRecordedAdjustments() throws {
    let session = try SettingsFixture.session()
    let viewModel = ProcessSessionDetailViewModel(session: session) { nil }
    let recorded: [SignalboxModelChangeAdjustment] = [.reasoningLevelClamped(from: .high, to: .low)]
    viewModel.apply(.event(.init(cursor: .init(rawValue: 1), sessionID: session.id,
      event: .turnModelSettingsResolved(adjustments: recorded))))

    XCTAssertEqual(viewModel.settingsAdjustments, recorded)
    XCTAssertEqual(viewModel.settingsAdjustments.map(\.settingsLabel), ["Reasoning adjusted from high to low"])
  }
}

/// Identities only correlate this fixture's session, model, and turn; their spellings are arbitrary.
private enum SettingsFixture {
  static let sessionID = try! SignalboxCanonicalUUID(validating: "11111111-1111-4111-8111-111111111111")
  static let selectionID = try! SignalboxCanonicalUUID(validating: "22222222-2222-4222-8222-222222222222")
  static let otherSelectionID = try! SignalboxCanonicalUUID(validating: "44444444-4444-4444-8444-444444444444")
  static let aliasID = try! SignalboxCanonicalUUID(validating: "55555555-5555-4555-8555-555555555555")
  static let turnID = try! SignalboxCanonicalUUID(validating: "33333333-3333-4333-8333-333333333333")
  static let systemPrompt = "Preserve this session prompt."
  static let input = "Continue with these settings."

  static func frame(_ message: [String: Any]) throws -> SignalboxProcessServerFrame {
    try SignalboxProcessServerFrame.decode(from: JSONSerialization.data(withJSONObject: [
      "version": 1, "request_id": "1", "message": message,
    ]))
  }

  static func defaults(reasoning: SignalboxSettingOverlay<SignalboxReasoningLevel>, version: String,
    type: String = "session_defaults", selectionID: SignalboxCanonicalUUID = SettingsFixture.selectionID,
    aliasID: SignalboxCanonicalUUID? = nil, fastMode: SignalboxFastModeOverlay = .inherit,
    serviceTier: SignalboxSettingOverlay<SignalboxServiceTier> = .inherit) throws -> SignalboxProcessServerFrame {
    let inherit: [String: Any] = ["kind": "inherit"]
    let inheritedLayer: [String: Any] = ["reasoning_level": inherit, "fast_mode": inherit, "service_tier": inherit]
    let sessionLayer = try JSONSerialization.jsonObject(with: SignalboxJSONCoding.encoder().encode(
      SignalboxModelSettingsOverlay(reasoningLevel: reasoning, fastMode: fastMode, serviceTier: serviceTier)))
    let effective: Any
    switch reasoning {
    case .value(let level): effective = level.rawValue
    case .inherit, .providerDefault: effective = NSNull()
    }
    let effectiveFastMode: SignalboxFastMode
    switch fastMode {
    case .inherit: effectiveFastMode = .disabled
    case .value(let value): effectiveFastMode = value
    }
    let effectiveTier: Any
    switch serviceTier {
    case .inherit, .providerDefault: effectiveTier = NSNull()
    case .value(let tier): effectiveTier = try JSONSerialization.jsonObject(with: SignalboxJSONCoding.encoder().encode(tier))
    }
    return try frame([
      "type": type, "session_id": sessionID.rawValue, "defaults_version": version,
      "model_selection": aliasID.map { ["kind": "alias", "alias_id": $0.rawValue] }
        ?? ["kind": "direct", "selection_id": selectionID.rawValue],
      "dangerous_tool_auto_approval": true, "system_prompt": systemPrompt,
      "model_settings": [
        "precedence": ["per_call": inheritedLayer, "session": sessionLayer,
          "profile": inheritedLayer, "global_default": inheritedLayer],
        "effective": ["reasoning_level": effective, "fast_mode": effectiveFastMode.rawValue, "service_tier": effectiveTier],
        "reasoning_source": reasoning == .inherit ? NSNull() : "session",
        "fast_mode_source": fastMode == .inherit ? NSNull() : "session",
        "service_tier_source": serviceTier == .inherit ? NSNull() : "session",
        "validated_for_selection_id": selectionID.rawValue,
      ],
    ])
  }

  static func session() throws -> SignalboxProcessSession {
    guard case .sessionDefaults(let defaults) = try defaults(reasoning: .inherit, version: "1").message else {
      throw SettingsFixtureError.missingDefaults
    }
    return .init(id: sessionID, defaults: defaults,
      metadata: .init(title: nil, tags: [], attributes: [:], archived: false))
  }

  /// Positions order the turns and supply arbitrary origin identities; settings are frozen at low reasoning.
  static func turn(position: UInt64, adjustedFromReasoning: SignalboxReasoningLevel?) throws -> SignalboxTranscriptTurn {
    let turnID = String(format: "00000000-0000-4000-8000-%012llx", position)
    let inputID = String(format: "11111111-1111-4111-8111-%012llx", (UInt64.max - position) & 0xffffffffffff)
    let inherited: [String: Any] = ["reasoning_level": ["kind": "inherit"],
      "fast_mode": ["kind": "inherit"], "service_tier": ["kind": "inherit"]]
    var session = inherited
    session["reasoning_level"] = ["kind": "value", "value": "low"]
    let adjustments = adjustedFromReasoning.map {
      [["type": "reasoning_level_clamped", "from": $0.rawValue, "to": "low"]]
    } ?? []
    let fields: [String: Any] = [
      "type": "transcript_turn", "turn_id": turnID, "acceptance_position": String(position),
      "state": ["type": "queued", "accepted_input_id": inputID,
        "content": [["type": "text", "text": input]]],
      "model_settings": ["turn_id": turnID, "accepted_input_id": inputID, "defaults_version": "1",
        "requested_model": ["kind": "direct", "selection_id": selectionID.rawValue],
        "selected_direct_id": selectionID.rawValue, "per_call_override": inherited,
        "settings": ["precedence": ["per_call": inherited, "session": session,
          "profile": inherited, "global_default": inherited],
          "effective": ["reasoning_level": "low", "fast_mode": "disabled", "service_tier": NSNull()],
          "reasoning_source": "session", "fast_mode_source": NSNull(), "service_tier_source": NSNull(),
          "validated_for_selection_id": selectionID.rawValue],
        "adjusted_from_selection_id": adjustedFromReasoning == nil ? NSNull() : otherSelectionID.rawValue,
        "adjustments": adjustments],
    ]
    return try SignalboxJSONCoding.decoder().decode(SignalboxTranscriptTurn.self,
      from: JSONSerialization.data(withJSONObject: fields))
  }
}

private enum SettingsFixtureError: Error { case missingDefaults, unexpectedRequest }

private actor SettingsRequester: SignalboxProcessRequesting {
  private var pages: [[SignalboxProcessServerFrame]]
  private var requests: [SignalboxProcessClientRequest] = []
  private var suspendsReplacement = false
  private var replacementStarted: CheckedContinuation<Void, Never>?
  private var replacementCompletion: CheckedContinuation<Void, Never>?

  init(pages: [[SignalboxProcessServerFrame]]) { self.pages = pages }

  init(reading: SignalboxSettingOverlay<SignalboxReasoningLevel>,
    replacement: SignalboxSettingOverlay<SignalboxReasoningLevel>? = nil,
    suspendsReplacement: Bool = false) throws {
    self.suspendsReplacement = suspendsReplacement
    pages = [
      [try SettingsFixture.defaults(reasoning: reading, version: "1")],
      [try SettingsFixture.frame(["type": "model_capabilities_start"]),
        try SettingsFixture.frame(["type": "model_capability_item", "selection_id": SettingsFixture.selectionID.rawValue,
          "capabilities": ["reasoning_levels": ["low", "high"], "fast_mode_supported": false, "service_tiers": []]]),
        try SettingsFixture.frame(["type": "model_capabilities_end", "capability_count": "1"])],
      [try SettingsFixture.frame(["type": "model_aliases_start"]),
        try SettingsFixture.frame(["type": "model_aliases_end", "alias_count": "0"])],
    ]
    if let replacement {
      pages.append([try SettingsFixture.defaults(reasoning: replacement, version: "2", type: "session_defaults_replaced")])
    }
  }

  func open(_ request: SignalboxProcessClientRequest) async throws -> any SignalboxProcessExchange {
    requests.append(request)
    if case .replaceSessionDefaults = request, suspendsReplacement {
      await withCheckedContinuation { continuation in
        replacementCompletion = continuation
        replacementStarted?.resume()
        replacementStarted = nil
      }
    }
    guard !pages.isEmpty else { throw SettingsFixtureError.unexpectedRequest }
    return SettingsExchange(frames: pages.removeFirst())
  }
  func lastRequest() -> SignalboxProcessClientRequest? { requests.last }
  func openedRequests() -> [SignalboxProcessClientRequest] { requests }
  func append(pages: [[SignalboxProcessServerFrame]]) { self.pages += pages }
  func waitUntilReplacementStarted() async {
    if replacementCompletion != nil { return }
    await withCheckedContinuation { replacementStarted = $0 }
  }
  func resumeReplacement() {
    replacementCompletion?.resume()
    replacementCompletion = nil
  }
}

private actor SettingsExchange: SignalboxProcessExchange {
  private var frames: [SignalboxProcessServerFrame]
  init(frames: [SignalboxProcessServerFrame]) { self.frames = frames }
  func next() async throws -> SignalboxProcessServerFrame? {
    frames.isEmpty ? nil : frames.removeFirst()
  }
  func close() async { frames = [] }
}
