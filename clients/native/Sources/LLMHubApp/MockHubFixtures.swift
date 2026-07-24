import Foundation

enum MockHubFixtures {
    static let activeSessionID = "11111111-1111-4111-8111-111111111111"
    static let approvalSessionID = "22222222-2222-4222-8222-222222222222"
    static let archivedSessionID = "33333333-3333-4333-8333-333333333333"
    static let createdSessionID = "44444444-4444-4444-8444-444444444444"
    static let failedSessionID = "55555555-5555-4555-8555-555555555555"
    static let markdownSessionID = "66666666-6666-4666-8666-666666666666"
    static let markdownBasicsSessionID = "77777777-7777-4777-8777-777777777777"
    static let markdownTableSessionID = "88888888-8888-4888-8888-888888888888"
    static let markdownCodeSessionID = "99999999-9999-4999-8999-999999999998"
    static let invocationID = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"

    static let templates = """
    {
      "templates": [
        {
          "id": "general_chat",
          "title": "General Chat",
          "description": "Broad assistant with safe tools.",
          "system_prompt": "You are a helpful assistant.",
          "default_tools": ["echo", "current_time", "save_report"],
          "default_model_alias": "claude-sonnet-latest",
          "default_tool_permission_overrides": {},
          "default_dangerous_auto_approve_all_tools": false,
          "is_builtin": true,
          "is_archived": false,
          "created_at": "2026-05-10T12:00:00Z",
          "last_modified_at": "2026-05-10T12:00:00Z"
        },
        {
          "id": "coder",
          "title": "Coder",
          "description": "Workspace-aware coding agent with gated writes.",
          "system_prompt": "You are a coding assistant.",
          "default_tools": ["read_file", "list_files", "write_file", "bash", "save_report"],
          "default_model_alias": "claude-sonnet-latest",
          "default_tool_permission_overrides": {"bash": "confirm", "write_file": "confirm"},
          "default_dangerous_auto_approve_all_tools": false,
          "is_builtin": true,
          "is_archived": false,
          "created_at": "2026-05-10T12:00:00Z",
          "last_modified_at": "2026-05-10T12:00:00Z"
        }
      ]
    }
    """

    static let runners = """
    {
      "runners": [
        {
          "id": "local-runner",
          "environment_tag": "laptop",
          "status": "online",
          "registered_at": "2026-05-10T12:00:00Z",
          "last_seen_at": "2026-05-10T12:04:00Z",
          "tool_catalog": [
            {"name": "echo", "description": "Echo text.", "json_schema": {"type": "object"}, "default_permission": "auto"},
            {"name": "bash", "description": "Run a shell command in the workspace.", "json_schema": {"type": "object"}, "default_permission": "confirm"},
            {"name": "write_file", "description": "Write a file in the workspace.", "json_schema": {"type": "object"}, "default_permission": "confirm"},
            {"name": "save_report", "description": "Save a markdown report artifact.", "json_schema": {"type": "object"}, "default_permission": "auto"}
          ]
        },
        {
          "id": "modal-sandbox",
          "environment_tag": "sandbox",
          "status": "offline",
          "registered_at": "2026-05-10T11:00:00Z",
          "last_seen_at": "2026-05-10T11:35:00Z",
          "tool_catalog": [
            {"name": "bash", "description": "Sandboxed shell.", "json_schema": {"type": "object"}, "default_permission": "confirm"}
          ]
        }
      ]
    }
    """

    static let monitor = """
    {
      "sessions": [
        {
          "metadata": \(sessionJSON(id: approvalSessionID, title: "Approve repo patch", status: "waiting_for_confirmation", archived: false, runnerID: "local-runner")),
          "status": {"state": "waiting_for_confirmation", "current_tool_calls": ["call-bash-1"], "status_updates": ["bash is waiting for approval"], "pending_user_messages": 0},
          "runner_status": "online",
          "event_count": 4,
          "message_count": 2,
          "tool_invocation_count": 1,
          "artifact_count": 0,
          "last_event_id": 3,
          "last_event_at": "2026-05-10T12:03:00Z",
          "last_activity_at": "2026-05-10T12:03:00Z",
          "last_event_kind": "tool_invocation"
        },
        {
          "metadata": \(sessionJSON(id: activeSessionID, title: "Research runner status", status: "prompting", archived: false, runnerID: "local-runner")),
          "status": {"state": "prompting", "current_tool_calls": [], "status_updates": ["streaming answer"], "pending_user_messages": 0},
          "runner_status": "online",
          "event_count": 6,
          "message_count": 4,
          "tool_invocation_count": 1,
          "artifact_count": 1,
          "last_event_id": 6,
          "last_event_at": "2026-05-10T12:02:00Z",
          "last_activity_at": "2026-05-10T12:02:00Z",
          "last_event_kind": "artifact_created"
        },
        {
          "metadata": \(sessionJSON(id: failedSessionID, title: "Failed workspace write", status: "failed", archived: false, runnerID: "local-runner")),
          "status": {"state": "failed", "current_tool_calls": [], "status_updates": ["write_file failed after path validation"], "pending_user_messages": 0, "reason": "Path is outside the workspace.", "failed_at": "2026-05-10T12:05:00Z"},
          "runner_status": "online",
          "event_count": 4,
          "message_count": 3,
          "tool_invocation_count": 1,
          "artifact_count": 0,
          "last_event_id": 4,
          "last_event_at": "2026-05-10T12:05:00Z",
          "last_activity_at": "2026-05-10T12:05:00Z",
          "last_event_kind": "turn_failed"
        }
      ],
      "limit": 100,
      "offset": 0,
      "total": 3
    }
    """

    static let initial = """
    {
      "sessions": [
        \(sessionJSON(id: approvalSessionID, title: "Approve repo patch", status: "waiting_for_confirmation", archived: false, runnerID: "local-runner")),
        \(sessionJSON(id: activeSessionID, title: "Research runner status", status: "prompting", archived: false, runnerID: "local-runner")),
        \(sessionJSON(id: markdownBasicsSessionID, title: "Markdown headings and lists", status: "idle", archived: false, runnerID: "local-runner")),
        \(sessionJSON(id: markdownTableSessionID, title: "Markdown tables and links", status: "idle", archived: false, runnerID: "local-runner")),
        \(sessionJSON(id: markdownCodeSessionID, title: "Markdown code and quotes", status: "idle", archived: false, runnerID: "local-runner")),
        \(sessionJSON(id: markdownSessionID, title: "Markdown incident report", status: "idle", archived: false, runnerID: "local-runner")),
        \(sessionJSON(id: failedSessionID, title: "Failed workspace write", status: "failed", archived: false, runnerID: "local-runner")),
        \(sessionJSON(id: archivedSessionID, title: "Archived smoke run", status: "idle", archived: true, runnerID: nil))
      ],
      "events_by_session": {
        "\(activeSessionID)": \(activeEvents),
        "\(approvalSessionID)": \(pendingToolEvents),
        "\(failedSessionID)": \(failedToolEvents),
        "\(markdownBasicsSessionID)": \(markdownBasicsEvents),
        "\(markdownTableSessionID)": \(markdownTableEvents),
        "\(markdownCodeSessionID)": \(markdownCodeEvents),
        "\(markdownSessionID)": \(markdownEvents)
      },
      "artifacts_by_session": {
        "\(activeSessionID)": [
          {
            "id": "99999999-9999-4999-8999-999999999999",
            "session_id": "\(activeSessionID)",
            "event_id": 6,
            "kind": "report",
            "title": "runner-status.md",
            "mime_type": "text/markdown",
            "path": null,
            "content_text": "# Runner Status\\n\\nThe local runner is online and has confirm-gated bash/write_file tools.",
            "metadata": {"tool": "save_report"},
            "created_at": "2026-05-10T12:02:15Z"
          }
        ],
        "\(approvalSessionID)": [],
        "\(failedSessionID)": [],
        "\(markdownBasicsSessionID)": [],
        "\(markdownTableSessionID)": [],
        "\(markdownCodeSessionID)": [],
        "\(markdownSessionID)": []
      }
    }
    """

    static let createdSession = """
    {
      "metadata": \(sessionJSON(id: createdSessionID, title: "New native session", status: "idle", archived: false, runnerID: "local-runner")),
      "status": {"state": "idle"}
    }
    """

    static func appendedUserMessage(text: String) -> String {
        let escaped = jsonEscaped(text)
        return """
        {
          "event_id": 20,
          "event": {
            "kind": "message",
            "message": {"role": "user", "parts": [{"kind": "text", "text": "\(escaped)"}]},
            "visible_to_llm": true,
            "visible_to_user": true,
            "is_streaming": false,
            "parent_tool_invocation": null,
            "created_at": "2026-05-10T12:06:00Z",
            "last_modified_at": "2026-05-10T12:06:00Z",
            "created_from": "app:apple-native"
          },
          "session_status": {"state": "prompting", "current_tool_calls": [], "status_updates": ["queued"], "pending_user_messages": 0}
        }
        """
    }

    static let streamMessages: [String] = [
        """
        {"kind":"status_changed","status":{"state":"prompting","current_tool_calls":[],"status_updates":["runner accepted turn"],"pending_user_messages":0}}
        """,
        """
        {"kind":"event_appended","event_id":21,"event":{"kind":"message","message":{"role":"assistant","parts":[{"kind":"text","text":"I am checking the runner fleet and current task state."}]},"visible_to_llm":true,"visible_to_user":true,"is_streaming":true,"parent_tool_invocation":null,"created_at":"2026-05-10T12:06:01Z","last_modified_at":"2026-05-10T12:06:01Z","created_from":"runner:local-runner"}}
        """,
        """
        {"kind":"event_updated","event_id":21,"event":{"kind":"message","message":{"role":"assistant","parts":[{"kind":"text","text":"I am checking the runner fleet and current task state. The local runner is online."}]},"visible_to_llm":true,"visible_to_user":true,"is_streaming":false,"parent_tool_invocation":null,"created_at":"2026-05-10T12:06:01Z","last_modified_at":"2026-05-10T12:06:02Z","created_from":"runner:local-runner"}}
        """,
        """
        {"kind":"status_changed","status":{"state":"idle"}}
        """
    ]

    static let pendingToolEvents = """
    [
      {"event_id":1,"event":{"kind":"message","message":{"role":"user","parts":[{"kind":"text","text":"Run the repo tests and patch the failing command."}]},"visible_to_llm":true,"visible_to_user":true,"is_streaming":false,"parent_tool_invocation":null,"created_at":"2026-05-10T12:02:00Z","last_modified_at":"2026-05-10T12:02:00Z","created_from":"app:apple-native"}},
      {"event_id":2,"event":{"kind":"message","message":{"role":"assistant","parts":[{"kind":"function_call","name":"bash","arguments":"{\\\"cmd\\\":\\\"bazel test //projects/llm_hub/...\\\"}","call_id":"call-bash-1"}]},"visible_to_llm":true,"visible_to_user":true,"is_streaming":false,"parent_tool_invocation":null,"created_at":"2026-05-10T12:02:30Z","last_modified_at":"2026-05-10T12:02:30Z","created_from":"runner:local-runner"}},
      {"event_id":3,"event":{"kind":"tool_invocation","invocation_id":"\(invocationID)","tool_name":"bash","tool_call_id":"call-bash-1","function_call_event_id":2,"function_response_event_id":null,"status_updates":["Requires confirmation before running shell command"],"pending_confirmation":true,"decision":null,"decision_at":null,"decision_reason":null,"is_collapsed_by_owner":false,"child_session_id":null,"last_modified_at":"2026-05-10T12:03:00Z"}}
    ]
    """

    static let approvedToolEvents = """
    [
      {"event_id":1,"event":{"kind":"message","message":{"role":"user","parts":[{"kind":"text","text":"Run the repo tests and patch the failing command."}]},"visible_to_llm":true,"visible_to_user":true,"is_streaming":false,"parent_tool_invocation":null,"created_at":"2026-05-10T12:02:00Z","last_modified_at":"2026-05-10T12:02:00Z","created_from":"app:apple-native"}},
      {"event_id":2,"event":{"kind":"message","message":{"role":"assistant","parts":[{"kind":"function_call","name":"bash","arguments":"{\\\"cmd\\\":\\\"bazel test //projects/llm_hub/...\\\"}","call_id":"call-bash-1"}]},"visible_to_llm":true,"visible_to_user":true,"is_streaming":false,"parent_tool_invocation":null,"created_at":"2026-05-10T12:02:30Z","last_modified_at":"2026-05-10T12:02:30Z","created_from":"runner:local-runner"}},
      {"event_id":3,"event":{"kind":"tool_invocation","invocation_id":"\(invocationID)","tool_name":"bash","tool_call_id":"call-bash-1","function_call_event_id":2,"function_response_event_id":4,"result":"succeeded","status_updates":["Approved by native client","Command completed"],"pending_confirmation":false,"decision":"approved","decision_at":"2026-05-10T12:03:30Z","decision_reason":null,"is_collapsed_by_owner":false,"child_session_id":null,"last_modified_at":"2026-05-10T12:03:45Z"}},
      {"event_id":4,"event":{"kind":"message","message":{"role":"tool","parts":[{"kind":"function_response","call_id":"call-bash-1","output":"3 tests passed. 1 target built from cache."}]},"visible_to_llm":true,"visible_to_user":true,"is_streaming":false,"parent_tool_invocation":"\(invocationID)","created_at":"2026-05-10T12:03:45Z","last_modified_at":"2026-05-10T12:03:45Z","created_from":"runner:local-runner"}}
    ]
    """

    static let deniedToolEvents = """
    [
      {"event_id":1,"event":{"kind":"message","message":{"role":"user","parts":[{"kind":"text","text":"Run the repo tests and patch the failing command."}]},"visible_to_llm":true,"visible_to_user":true,"is_streaming":false,"parent_tool_invocation":null,"created_at":"2026-05-10T12:02:00Z","last_modified_at":"2026-05-10T12:02:00Z","created_from":"app:apple-native"}},
      {"event_id":2,"event":{"kind":"message","message":{"role":"assistant","parts":[{"kind":"function_call","name":"bash","arguments":"{\\\"cmd\\\":\\\"bazel test //projects/llm_hub/...\\\"}","call_id":"call-bash-1"}]},"visible_to_llm":true,"visible_to_user":true,"is_streaming":false,"parent_tool_invocation":null,"created_at":"2026-05-10T12:02:30Z","last_modified_at":"2026-05-10T12:02:30Z","created_from":"runner:local-runner"}},
      {"event_id":3,"event":{"kind":"tool_invocation","invocation_id":"\(invocationID)","tool_name":"bash","tool_call_id":"call-bash-1","function_call_event_id":2,"function_response_event_id":4,"status_updates":["Denied by native client"],"pending_confirmation":false,"decision":"denied","decision_at":"2026-05-10T12:03:30Z","decision_reason":"Review command scope first.","is_collapsed_by_owner":false,"child_session_id":null,"last_modified_at":"2026-05-10T12:03:45Z"}},
      {"event_id":4,"event":{"kind":"message","message":{"role":"tool","parts":[{"kind":"function_response","call_id":"call-bash-1","output":"Tool call denied: Review command scope first."}]},"visible_to_llm":true,"visible_to_user":true,"is_streaming":false,"parent_tool_invocation":"\(invocationID)","created_at":"2026-05-10T12:03:45Z","last_modified_at":"2026-05-10T12:03:45Z","created_from":"runner:local-runner"}}
    ]
    """

    static let activeEvents = """
    [
      {"event_id":1,"event":{"kind":"message","message":{"role":"user","parts":[{"kind":"text","text":"Summarize the runner state."}]},"visible_to_llm":true,"visible_to_user":true,"is_streaming":false,"parent_tool_invocation":null,"created_at":"2026-05-10T12:00:00Z","last_modified_at":"2026-05-10T12:00:00Z","created_from":"app:apple-native"}},
      {"event_id":2,"event":{"kind":"message","message":{"role":"assistant","parts":[{"kind":"text","text":"The local runner is online and the sandbox runner is offline."}]},"visible_to_llm":true,"visible_to_user":true,"is_streaming":false,"parent_tool_invocation":null,"created_at":"2026-05-10T12:01:00Z","last_modified_at":"2026-05-10T12:01:00Z","created_from":"runner:local-runner"}},
      {"event_id":5,"event":{"kind":"message","message":{"role":"assistant","parts":[{"kind":"function_call","name":"save_report","arguments":"{\\\"title\\\":\\\"runner-status.md\\\"}","call_id":"call-save-report-1"}]},"visible_to_llm":true,"visible_to_user":true,"is_streaming":false,"parent_tool_invocation":null,"created_at":"2026-05-10T12:02:00Z","last_modified_at":"2026-05-10T12:02:00Z","created_from":"runner:local-runner"}},
      {"event_id":6,"event":{"kind":"tool_invocation","invocation_id":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb","tool_name":"save_report","tool_call_id":"call-save-report-1","function_call_event_id":5,"function_response_event_id":7,"result":"succeeded","status_updates":["Saved markdown report"],"pending_confirmation":false,"decision":null,"decision_at":null,"decision_reason":null,"is_collapsed_by_owner":false,"child_session_id":null,"last_modified_at":"2026-05-10T12:02:10Z"}},
      {"event_id":7,"event":{"kind":"message","message":{"role":"tool","parts":[{"kind":"function_response","call_id":"call-save-report-1","output":"Saved artifact runner-status.md"}]},"visible_to_llm":true,"visible_to_user":true,"is_streaming":false,"parent_tool_invocation":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb","created_at":"2026-05-10T12:02:10Z","last_modified_at":"2026-05-10T12:02:10Z","created_from":"runner:local-runner"}}
    ]
    """

    static let failedToolEvents = """
    [
      {"event_id":1,"event":{"kind":"message","message":{"role":"user","parts":[{"kind":"text","text":"Write the report outside the workspace."}]},"visible_to_llm":true,"visible_to_user":true,"is_streaming":false,"parent_tool_invocation":null,"created_at":"2026-05-10T12:04:00Z","last_modified_at":"2026-05-10T12:04:00Z","created_from":"app:apple-native"}},
      {"event_id":2,"event":{"kind":"message","message":{"role":"assistant","parts":[{"kind":"function_call","name":"write_file","arguments":"{\\\"path\\\":\\\"/etc/report.md\\\",\\\"content\\\":\\\"summary\\\"}","call_id":"call-write-file-1"}]},"visible_to_llm":true,"visible_to_user":true,"is_streaming":false,"parent_tool_invocation":null,"created_at":"2026-05-10T12:04:15Z","last_modified_at":"2026-05-10T12:04:15Z","created_from":"runner:local-runner"}},
      {"event_id":3,"event":{"kind":"tool_invocation","invocation_id":"cccccccc-cccc-4ccc-8ccc-cccccccccccc","tool_name":"write_file","tool_call_id":"call-write-file-1","function_call_event_id":2,"function_response_event_id":4,"result":"failed","status_updates":["Path validation failed"],"pending_confirmation":false,"decision":null,"decision_at":null,"decision_reason":null,"is_collapsed_by_owner":false,"child_session_id":null,"last_modified_at":"2026-05-10T12:04:30Z"}},
      {"event_id":4,"event":{"kind":"message","message":{"role":"tool","parts":[{"kind":"function_response","call_id":"call-write-file-1","output":"failed: path is outside the workspace root"}]},"visible_to_llm":true,"visible_to_user":true,"is_streaming":false,"parent_tool_invocation":"cccccccc-cccc-4ccc-8ccc-cccccccccccc","created_at":"2026-05-10T12:04:30Z","last_modified_at":"2026-05-10T12:04:30Z","created_from":"runner:local-runner"}},
      {"event_id":5,"event":{"kind":"turn_failed","turn_id":"turn-failed-write","reason":"Path is outside the workspace.","failed_at":"2026-05-10T12:05:00Z","runner_id":"local-runner","visible_to_llm":true,"visible_to_user":true,"created_at":"2026-05-10T12:05:00Z","last_modified_at":"2026-05-10T12:05:00Z","created_from":"runner:local-runner"}}
    ]
    """

    static let markdownBasicsEvents = messageEventsJSON(
        userText: "Show headings, emphasis, ordered lists, unordered lists, and task states.",
        assistantText: """
        # Incident Handoff

        ## Current status

        The **local hub** is connected, the _native client_ is rendering Markdown, and `runner-docker` is available.

        ### Operator checklist

        - Confirm the runner is still online.
        - Review **pending approvals** before executing tools.
        - Keep `write_file` gated unless the workspace is trusted.

        1. Open the session timeline.
        2. Inspect the latest tool card.
        3. Archive the session after the artifact is reviewed.

        - [x] Validate connection settings.
        - [x] Capture screenshots.
        - [ ] Re-run the local smoke after review.
        """
    )

    static let markdownTableEvents = messageEventsJSON(
        userText: "Render a status table with links and inline code.",
        assistantText: """
        ## Runner fleet summary

        | Runner | Environment | Status | Permissions | Runbook |
        | --- | --- | --- | --- | --- |
        | `local-runner` | laptop | **online** | `bash: confirm` | [Local ops](https://example.invalid/runbooks/local-runner) |
        | `modal-sandbox` | sandbox | offline | `bash: confirm` | [Sandbox guide](https://example.invalid/runbooks/sandbox) |
        | `remote-gpu-a` | gpu | degraded | `fetch_url: auto` | [Escalation](https://example.invalid/runbooks/gpu) |

        The table should stay horizontally scrollable on compact phones without squeezing the timeline.
        """
    )

    static let markdownCodeEvents = messageEventsJSON(
        userText: "Render quoted notes and several fenced code blocks.",
        assistantText: """
        ## Tool approval notes

        > Approval should be obvious, inspectable, and reversible in the timeline.
        > Never log API keys in command output or diagnostic cards.

        ```bash
        curl -sS "http://127.0.0.1:8000/health"
        just llm-hub-native-smoke
        ```

        ```json
        {
          "tool": "bash",
          "permission": "confirm",
          "decision": "approved"
        }
        ```

        ```swift
        let status = HubSessionStatus.State.waitingForConfirmation
        ```

        ---

        Inline code such as `HubToolInvocationID` should align with surrounding prose.
        """
    )

    static let markdownEvents = messageEventsJSON(
        userText: "Summarize the last local smoke run as a mixed markdown report.",
        assistantText: """
        # Local smoke result

        The **macOS client** connected to `http://127.0.0.1:8000` and used the native hub API.

        | Step | Result | Evidence |
        | --- | --- | --- |
        | Fetch sessions | **Pass** | `GET /api/sessions` |
        | Create session | **Pass** | `POST /api/sessions` |
        | Tool approval | Needs review | `bash` confirmation card |

        ## Follow-ups

        - Inspect [verification report](https://example.invalid/llm-hub-native/report).
        - Keep large tool output scrollable.
        - Validate artifact previews on iPad landscape.

        ```text
        exit_code=0
        native-approval-smoke
        ```
        """
    )

    private static func messageEventsJSON(userText: String, assistantText: String) -> String {
        """
        [
          {"event_id":1,"event":{"kind":"message","message":{"role":"user","parts":[{"kind":"text","text":"\(jsonEscaped(userText))"}]},"visible_to_llm":true,"visible_to_user":true,"is_streaming":false,"parent_tool_invocation":null,"created_at":"2026-05-10T12:08:00Z","last_modified_at":"2026-05-10T12:08:00Z","created_from":"app:apple-native"}},
          {"event_id":2,"event":{"kind":"message","message":{"role":"assistant","parts":[{"kind":"text","text":"\(jsonEscaped(assistantText))"}]},"visible_to_llm":true,"visible_to_user":true,"is_streaming":false,"parent_tool_invocation":null,"created_at":"2026-05-10T12:08:20Z","last_modified_at":"2026-05-10T12:08:20Z","created_from":"runner:local-runner"}}
        ]
        """
    }

    private static func jsonEscaped(_ value: String) -> String {
        value
            .replacingOccurrences(of: "\\", with: "\\\\")
            .replacingOccurrences(of: "\"", with: "\\\"")
            .replacingOccurrences(of: "\n", with: "\\n")
            .replacingOccurrences(of: "\r", with: "\\r")
            .replacingOccurrences(of: "\t", with: "\\t")
    }

    private static func sessionJSON(
        id: String,
        title: String,
        status: String,
        archived: Bool,
        runnerID: String?
    ) -> String {
        let runnerValue = runnerID.map { "\"\($0)\"" } ?? "null"
        return """
        {
          "id": "\(id)",
          "created_from_template_id": "coder",
          "title": "\(title)",
          "description": null,
          "system_prompt": "You are a native-client fixture assistant.",
          "model_alias": "claude-sonnet-latest",
          "enabled_tools": ["echo", "bash", "write_file", "save_report"],
          "tool_permission_overrides": {"bash": "confirm", "write_file": "confirm"},
          "dangerous_auto_approve_all_tools": false,
          "runner_id": \(runnerValue),
          "tags": ["native", "\(status)"],
          "is_archived": \(archived ? "true" : "false"),
          "parent_session_id": null,
          "linked_workspace_path": "/tmp/llm-hub-native",
          "created_at": "2026-05-10T12:00:00Z",
          "last_modified_at": "2026-05-10T12:04:00Z",
          "last_turn_ended_at": null,
          "created_from": "app:apple-native",
          "last_prompted_from": "app:apple-native",
          "source_app": "apple-native"
        }
        """
    }
}
