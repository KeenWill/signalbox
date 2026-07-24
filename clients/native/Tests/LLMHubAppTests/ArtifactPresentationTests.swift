import Foundation
@testable import LLMHubApp
import LLMHubModels
import XCTest

final class ArtifactPresentationTests: XCTestCase {
    func testPromptContextArtifactUsesDedicatedPresentation() throws {
        let artifact = try makeArtifact(
            kind: "prompt_context",
            title: "prompt-context.json",
            mimeType: "text/markdown",
            contentText: "assembled system prompt"
        )

        XCTAssertTrue(artifact.isPromptContextArtifact)
        XCTAssertEqual(artifact.presentationTitle, "Prompt Context")
        XCTAssertEqual(artifact.presentationKindLabel, "Prompt context")
        XCTAssertEqual(artifact.presentationSubtitle, "Prompt context  text/markdown")
        XCTAssertEqual(artifact.presentationSystemImageName, "brain.head.profile")
        XCTAssertEqual(
            artifact.presentationAccessibilityLabel,
            "Prompt Context, Prompt context  text/markdown"
        )
        XCTAssertEqual(artifact.previewNavigationTitle, "Prompt Context")
        XCTAssertTrue(artifact.prefersMonospacedPreview)
        XCTAssertTrue(artifact.usesMonospacedPreviewText)
    }

    func testReportArtifactKeepsOriginalTitle() throws {
        let artifact = try makeArtifact(
            kind: "report",
            title: "runner-status.md",
            mimeType: "text/markdown",
            contentText: "# Runner Status"
        )

        XCTAssertFalse(artifact.isPromptContextArtifact)
        XCTAssertEqual(artifact.presentationTitle, "runner-status.md")
        XCTAssertEqual(artifact.presentationKindLabel, "Report")
        XCTAssertEqual(artifact.presentationSubtitle, "Report  text/markdown")
        XCTAssertEqual(artifact.presentationSystemImageName, "doc.text")
        XCTAssertEqual(artifact.previewNavigationTitle, "Artifact")
        XCTAssertFalse(artifact.prefersMonospacedPreview)
    }

    func testUnknownArtifactKindGetsReadableLabel() throws {
        let artifact = try makeArtifact(
            kind: "task_summary",
            title: "tasks.json",
            mimeType: "application/json",
            contentText: "{}"
        )

        XCTAssertEqual(artifact.presentationKindLabel, "Task Summary")
        XCTAssertEqual(artifact.presentationSystemImageName, "shippingbox")
        XCTAssertTrue(artifact.prefersMonospacedPreview)
    }

    func testMarkdownArtifactWithFencedCodeUsesMonospacedPreviewText() throws {
        let artifact = try makeArtifact(
            kind: "report",
            title: "runner-status.md",
            mimeType: "text/markdown",
            contentText: """
            # Runner Status

            ```json
            {"status":"ok"}
            ```
            """
        )

        XCTAssertTrue(artifact.prefersMonospacedPreview)
    }

    func testSystemPromptArtifactUsesMonospacedPreview() throws {
        let artifact = try makeArtifact(
            kind: "system_prompt",
            title: "Assembled System Prompt",
            mimeType: "text/plain",
            contentText: "system instructions"
        )

        XCTAssertFalse(artifact.isPromptContextArtifact)
        XCTAssertEqual(artifact.presentationTitle, "Assembled System Prompt")
        XCTAssertEqual(artifact.presentationSystemImageName, "text.quote")
        XCTAssertTrue(artifact.prefersMonospacedPreview)
    }

    func testBlankTitleFallsBackToKind() throws {
        let artifact = try makeArtifact(
            kind: "status_report",
            title: "   ",
            mimeType: nil,
            contentText: "status"
        )

        XCTAssertEqual(artifact.presentationTitle, "status_report")
        XCTAssertEqual(artifact.presentationSubtitle, "Status Report  unknown type")
    }

    private func makeArtifact(
        kind: String,
        title: String,
        mimeType: String?,
        contentText: String
    ) throws -> HubArtifact {
        let payload: [String: Any] = [
            "id": "99999999-9999-4999-8999-999999999999",
            "session_id": "11111111-1111-4111-8111-111111111111",
            "event_id": 6,
            "kind": kind,
            "title": title,
            "mime_type": mimeType.map { $0 as Any } ?? NSNull(),
            "path": NSNull(),
            "content_text": contentText,
            "metadata": [String: Any](),
            "created_at": "2026-05-10T12:02:15Z",
        ]
        let data = try JSONSerialization.data(withJSONObject: payload)
        return try HubJSONCoding.decoder().decode(HubArtifact.self, from: data)
    }
}
