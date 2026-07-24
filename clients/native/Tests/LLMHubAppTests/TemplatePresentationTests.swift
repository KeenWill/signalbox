import Foundation
@testable import LLMHubApp
import LLMHubModels
import XCTest

final class TemplatePresentationTests: XCTestCase {
    func testStaleTemplateShowsSourceVersionAdvisory() throws {
        let template = try makeTemplate(
            derivativeStatus: "stale",
            currentSourceTemplateVersion: "2026-06-05",
            currentSourceTemplatePromptVersion: "2026-06-04"
        )

        XCTAssertEqual(template.visibleDerivativeStatus, .stale)
        XCTAssertEqual(template.versionSummary, "template local-1, prompt local-prompt-1")
        XCTAssertEqual(
            template.derivativeAdvisoryText,
            "Derived from coder; source has changed to template 2026-06-05, prompt 2026-06-04."
        )
    }

    func testCurrentTemplateShowsCurrentAdvisory() throws {
        let template = try makeTemplate(derivativeStatus: "current")

        XCTAssertEqual(template.visibleDerivativeStatus, .current)
        XCTAssertEqual(template.derivativeAdvisoryText, "Derived from coder; source is current.")
    }

    func testNonDerivedTemplateHidesAdvisory() throws {
        let template = try makeTemplate(
            templateVersion: nil,
            promptVersion: "prompt-only",
            derivedFromTemplateID: nil,
            derivativeStatus: "not_derived"
        )

        XCTAssertNil(template.visibleDerivativeStatus)
        XCTAssertEqual(template.versionSummary, "prompt prompt-only")
        XCTAssertNil(template.derivativeAdvisoryText)
    }

    private func makeTemplate(
        templateVersion: String? = "local-1",
        promptVersion: String? = "local-prompt-1",
        derivedFromTemplateID: String? = "coder",
        derivativeStatus: String,
        currentSourceTemplateVersion: String? = "2026-06-04",
        currentSourceTemplatePromptVersion: String? = "2026-06-04"
    ) throws -> HubTemplate {
        let payload: [String: Any] = [
            "id": "coder-custom",
            "title": "Coder Custom",
            "description": "Personal coder template.",
            "template_version": templateVersion.map { $0 as Any } ?? NSNull(),
            "prompt_version": promptVersion.map { $0 as Any } ?? NSNull(),
            "derived_from_template_id": derivedFromTemplateID.map { $0 as Any } ?? NSNull(),
            "derived_from_template_version": "2026-06-04",
            "derived_from_template_prompt_version": "2026-06-04",
            "derivative_status": derivativeStatus,
            "current_source_template_version": currentSourceTemplateVersion.map { $0 as Any } ?? NSNull(),
            "current_source_template_prompt_version": currentSourceTemplatePromptVersion.map { $0 as Any } ?? NSNull(),
            "system_prompt": "You are a coding assistant.",
            "default_tools": ["read_file", "bash"],
            "default_model_alias": "claude-sonnet-latest",
            "default_tool_permission_overrides": ["bash": "confirm"],
            "default_dangerous_auto_approve_all_tools": false,
            "is_builtin": false,
            "is_archived": false,
            "created_at": "2026-05-10T12:00:00Z",
            "last_modified_at": "2026-06-04T12:00:00Z",
        ]
        let data = try JSONSerialization.data(withJSONObject: payload)
        return try HubJSONCoding.decoder().decode(HubTemplate.self, from: data)
    }
}
