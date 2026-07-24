import LLMHubModels
import XCTest

final class HubModelsTests: XCTestCase {
    func testSessionStatusDecodesWaitingForConfirmation() throws {
        let data = Data(
            #"{"state":"waiting_for_confirmation","current_tool_calls":["call-1"],"status_updates":["approval needed"],"pending_user_messages":1}"#.utf8
        )

        let status = try HubJSONCoding.decoder().decode(HubSessionStatus.self, from: data)

        XCTAssertEqual(status.state, .waitingForConfirmation)
        XCTAssertEqual(status.label, "Needs Approval")
        XCTAssertEqual(status.currentToolCalls, ["call-1"])
        XCTAssertEqual(status.pendingUserMessages, 1)
    }

    func testUnknownEventDecodesWithoutCrashing() throws {
        let data = Data(
            #"{"event_id":9,"event":{"kind":"future_event","field":"value"}}"#.utf8
        )

        let event = try HubJSONCoding.decoder().decode(HubStoredEvent.self, from: data)

        guard case .unknown(let unknown) = event.event else {
            return XCTFail("Expected unknown event")
        }
        XCTAssertEqual(unknown.kind, "future_event")
        XCTAssertEqual(unknown.payload["field"], .string("value"))
    }

    func testUnknownRunnerStatusDecodesToFallback() throws {
        let data = Data(#""sleeping""#.utf8)

        let status = try HubJSONCoding.decoder().decode(HubRunnerStatus.self, from: data)

        XCTAssertEqual(status, .unknown)
    }

    func testEventNormalizerBuildsToolCardFromLinkedEvents() throws {
        let events = try HubJSONCoding.decoder().decode([HubStoredEvent].self, from: Data(Self.toolEventsJSON.utf8))

        let timeline = HubEventNormalizer.normalize(events)

        XCTAssertEqual(timeline.count, 2)
        guard case .tool(let tool) = timeline[1] else {
            return XCTFail("Expected tool card")
        }
        XCTAssertEqual(tool.toolName, "bash")
        XCTAssertEqual(tool.status, .succeeded)
        XCTAssertEqual(tool.arguments, #"{"cmd":"pwd"}"#)
        XCTAssertEqual(tool.output, "/tmp/workspace")
    }

    func testEventNormalizerUsesNeutralCompletedStatusWithoutStructuredResult() throws {
        let events = try HubJSONCoding.decoder().decode(
            [HubStoredEvent].self,
            from: Data(Self.toolEventsWithoutResultJSON.utf8)
        )

        let timeline = HubEventNormalizer.normalize(events)

        guard case .tool(let tool) = timeline[1] else {
            return XCTFail("Expected tool card")
        }
        XCTAssertEqual(tool.status, .completed)
        XCTAssertEqual(tool.output, "0 failed checks; retry was not needed.")
    }

    func testTemplateDecodesDerivativeAdvisoryMetadata() throws {
        let data = Data(Self.staleDerivedTemplateJSON.utf8)

        let template = try HubJSONCoding.decoder().decode(HubTemplate.self, from: data)

        XCTAssertEqual(template.id, HubTemplateID(rawValue: "coder-custom"))
        XCTAssertEqual(template.templateVersion, "local-1")
        XCTAssertEqual(template.promptVersion, "local-prompt-1")
        XCTAssertEqual(template.derivedFromTemplateID, HubTemplateID(rawValue: "coder"))
        XCTAssertEqual(template.derivedFromTemplateVersion, "2026-06-03")
        XCTAssertEqual(template.derivedFromTemplatePromptVersion, "2026-06-03")
        XCTAssertEqual(template.derivativeStatus, .stale)
        XCTAssertEqual(template.currentSourceTemplateVersion, "2026-06-04")
        XCTAssertEqual(template.currentSourceTemplatePromptVersion, "2026-06-04")
    }

    func testTemplateDerivativeStatusPreservesUnknownValues() throws {
        let data = Data(#""source_replaced""#.utf8)

        let status = try HubJSONCoding.decoder().decode(HubTemplateDerivativeStatus.self, from: data)

        XCTAssertEqual(status, .unknown("source_replaced"))
        XCTAssertEqual(status.label, "source_replaced")
    }

    private static let toolEventsJSON = """
    [
      {"event_id":1,"event":{"kind":"message","message":{"role":"user","parts":[{"kind":"text","text":"where am I?"}]},"visible_to_llm":true,"visible_to_user":true,"is_streaming":false,"parent_tool_invocation":null,"created_at":"2026-05-10T12:00:00Z","last_modified_at":"2026-05-10T12:00:00Z","created_from":"test"}},
      {"event_id":2,"event":{"kind":"message","message":{"role":"assistant","parts":[{"kind":"function_call","name":"bash","arguments":"{\\\"cmd\\\":\\\"pwd\\\"}","call_id":"call-1"}]},"visible_to_llm":true,"visible_to_user":true,"is_streaming":false,"parent_tool_invocation":null,"created_at":"2026-05-10T12:00:01Z","last_modified_at":"2026-05-10T12:00:01Z","created_from":"runner:test"}},
      {"event_id":3,"event":{"kind":"tool_invocation","invocation_id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","tool_name":"bash","tool_call_id":"call-1","function_call_event_id":2,"function_response_event_id":4,"result":"succeeded","status_updates":["done"],"pending_confirmation":false,"decision":"approved","decision_at":"2026-05-10T12:00:02Z","decision_reason":null,"is_collapsed_by_owner":false,"child_session_id":null,"last_modified_at":"2026-05-10T12:00:02Z"}},
      {"event_id":4,"event":{"kind":"message","message":{"role":"tool","parts":[{"kind":"function_response","call_id":"call-1","output":"/tmp/workspace"}]},"visible_to_llm":true,"visible_to_user":true,"is_streaming":false,"parent_tool_invocation":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","created_at":"2026-05-10T12:00:02Z","last_modified_at":"2026-05-10T12:00:02Z","created_from":"runner:test"}}
    ]
    """

    private static let staleDerivedTemplateJSON = """
    {
      "id": "coder-custom",
      "title": "Coder Custom",
      "description": "Personal coder template.",
      "template_version": "local-1",
      "prompt_version": "local-prompt-1",
      "derived_from_template_id": "coder",
      "derived_from_template_version": "2026-06-03",
      "derived_from_template_prompt_version": "2026-06-03",
      "derivative_status": "stale",
      "current_source_template_version": "2026-06-04",
      "current_source_template_prompt_version": "2026-06-04",
      "system_prompt": "You are a coding assistant.",
      "default_tools": ["read_file", "bash"],
      "default_model_alias": "claude-sonnet-latest",
      "default_tool_permission_overrides": {"bash": "confirm"},
      "default_dangerous_auto_approve_all_tools": false,
      "is_builtin": false,
      "is_archived": false,
      "created_at": "2026-05-10T12:00:00Z",
      "last_modified_at": "2026-06-04T12:00:00Z"
    }
    """

    private static let toolEventsWithoutResultJSON = """
    [
      {"event_id":1,"event":{"kind":"message","message":{"role":"user","parts":[{"kind":"text","text":"run checks"}]},"visible_to_llm":true,"visible_to_user":true,"is_streaming":false,"parent_tool_invocation":null,"created_at":"2026-05-10T12:00:00Z","last_modified_at":"2026-05-10T12:00:00Z","created_from":"test"}},
      {"event_id":2,"event":{"kind":"message","message":{"role":"assistant","parts":[{"kind":"function_call","name":"bash","arguments":"{\\\"cmd\\\":\\\"run-checks\\\"}","call_id":"call-2"}]},"visible_to_llm":true,"visible_to_user":true,"is_streaming":false,"parent_tool_invocation":null,"created_at":"2026-05-10T12:00:01Z","last_modified_at":"2026-05-10T12:00:01Z","created_from":"runner:test"}},
      {"event_id":3,"event":{"kind":"tool_invocation","invocation_id":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb","tool_name":"bash","tool_call_id":"call-2","function_call_event_id":2,"function_response_event_id":4,"status_updates":["done"],"pending_confirmation":false,"decision":null,"decision_at":null,"decision_reason":null,"is_collapsed_by_owner":false,"child_session_id":null,"last_modified_at":"2026-05-10T12:00:02Z"}},
      {"event_id":4,"event":{"kind":"message","message":{"role":"tool","parts":[{"kind":"function_response","call_id":"call-2","output":"0 failed checks; retry was not needed."}]},"visible_to_llm":true,"visible_to_user":true,"is_streaming":false,"parent_tool_invocation":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb","created_at":"2026-05-10T12:00:02Z","last_modified_at":"2026-05-10T12:00:02Z","created_from":"runner:test"}}
    ]
    """
}
