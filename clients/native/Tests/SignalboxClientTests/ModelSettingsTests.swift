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
  static let turnID = try! SignalboxCanonicalUUID(validating: "33333333-3333-4333-8333-333333333333")
  static let systemPrompt = "Preserve this session prompt."
  static let input = "Continue with these settings."

  static func frame(_ message: [String: Any]) throws -> SignalboxProcessServerFrame {
    try SignalboxProcessServerFrame.decode(from: JSONSerialization.data(withJSONObject: [
      "version": 1, "request_id": "1", "message": message,
    ]))
  }

  static func defaults(reasoning: SignalboxSettingOverlay<SignalboxReasoningLevel>, version: String,
    type: String = "session_defaults") throws -> SignalboxProcessServerFrame {
    let inherit: [String: Any] = ["kind": "inherit"]
    let inheritedLayer: [String: Any] = ["reasoning_level": inherit, "fast_mode": inherit, "service_tier": inherit]
    let sessionLayer = try JSONSerialization.jsonObject(with: SignalboxJSONCoding.encoder().encode(
      SignalboxModelSettingsOverlay(reasoningLevel: reasoning, fastMode: .inherit, serviceTier: .inherit)))
    let effective: Any
    switch reasoning {
    case .value(let level): effective = level.rawValue
    case .inherit, .providerDefault: effective = NSNull()
    }
    return try frame([
      "type": type, "session_id": sessionID.rawValue, "defaults_version": version,
      "model_selection": ["kind": "direct", "selection_id": selectionID.rawValue],
      "dangerous_tool_auto_approval": true, "system_prompt": systemPrompt,
      "model_settings": [
        "precedence": ["per_call": inheritedLayer, "session": sessionLayer,
          "profile": inheritedLayer, "global_default": inheritedLayer],
        "effective": ["reasoning_level": effective, "fast_mode": "disabled", "service_tier": NSNull()],
        "reasoning_source": reasoning == .inherit ? NSNull() : "session",
        "fast_mode_source": NSNull(), "service_tier_source": NSNull(),
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
}

private enum SettingsFixtureError: Error { case missingDefaults, unexpectedRequest }

private actor SettingsRequester: SignalboxProcessRequesting {
  private var pages: [[SignalboxProcessServerFrame]]
  private var requests: [SignalboxProcessClientRequest] = []

  init(pages: [[SignalboxProcessServerFrame]]) { self.pages = pages }

  init(reading: SignalboxSettingOverlay<SignalboxReasoningLevel>,
    replacement: SignalboxSettingOverlay<SignalboxReasoningLevel>? = nil) throws {
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
    guard !pages.isEmpty else { throw SettingsFixtureError.unexpectedRequest }
    return SettingsExchange(frames: pages.removeFirst())
  }
  func lastRequest() -> SignalboxProcessClientRequest? { requests.last }
  func openedRequests() -> [SignalboxProcessClientRequest] { requests }
}

private actor SettingsExchange: SignalboxProcessExchange {
  private var frames: [SignalboxProcessServerFrame]
  init(frames: [SignalboxProcessServerFrame]) { self.frames = frames }
  func next() async throws -> SignalboxProcessServerFrame? {
    frames.isEmpty ? nil : frames.removeFirst()
  }
  func close() async { frames = [] }
}
