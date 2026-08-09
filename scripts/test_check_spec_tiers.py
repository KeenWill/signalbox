#!/usr/bin/env python3
"""Prove check_spec_tiers.py fails on a drifted tier and passes on a clean one.

A checker that cannot be shown to fail is the false-confidence trap this
repository documents, and a documentation checker is unusually exposed to it: a
version that returned zero unconditionally would look identical in CI to one
that works, because the enrolled pages satisfy it. Each rule therefore gets a
failing case and a passing one.

Two passing cases are the ones worth reading closely, because they are where a
blunter checker would have been turned off within a week. A specification page
says "a call with no present usage axis" about data, not about a build, so rule
5 must match an absence claim only where it opens a sentence. And an `Open
edges` section exists precisely to name unimplemented work, so a rule that
forbade naming it would make the repository's own documented convention
unsatisfiable.

Each case runs the checker as a subprocess against a synthetic `docs/spec` tree
in a temporary working directory, so its root-relative discovery sees only the
fixture and never this repository's real pages.
"""

from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

CHECKER = Path(__file__).resolve().parent / "check_spec_tiers.py"

# The checker's enrolled pages, which the fixture must supply both of.
CONFIG_PAGE = "docs/spec/configuration-and-credentials.md"
AVAILABILITY_PAGE = "docs/spec/credential-availability.md"

MINIMAL_AVAILABILITY = """# Credential availability

## The credential-availability machine

This build composes no credential pool.

### Committed unimplemented functionality — the machine

No present composition resolves a pool.
"""


def run_checker(pages: dict[str, str]) -> subprocess.CompletedProcess[str]:
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        for name, body in pages.items():
            path = root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(body, encoding="utf-8")
        return subprocess.run(
            [sys.executable, str(CHECKER)],
            cwd=root,
            capture_output=True,
            text=True,
            check=False,
        )


def check_config(body: str) -> subprocess.CompletedProcess[str]:
    return run_checker(
        {CONFIG_PAGE: body, AVAILABILITY_PAGE: MINIMAL_AVAILABILITY}
    )


class SpecTierCheckerTests(unittest.TestCase):
    def test_clean_page_passes(self) -> None:
        result = check_config(
            "# Configuration\n\n"
            "## Deliveries\n\n"
            "The daemon reads two fields.\n\n"
            "### Committed unimplemented functionality — pools\n\n"
            "No present composition parses a pool.\n"
        )

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("2 enrolled pages", result.stdout)

    def test_bold_tier_label_fails(self) -> None:
        result = check_config(
            "# Configuration\n\n"
            "## Deliveries\n\n"
            "**Committed unimplemented functionality — pools.** No present "
            "composition parses a pool.\n"
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("tier label written as bold prose", result.stdout)

    def test_bold_tier_label_inside_a_fence_passes(self) -> None:
        result = check_config(
            "# Configuration\n\n"
            "## Deliveries\n\n"
            "The daemon reads two fields.\n\n"
            "```markdown\n"
            "**Committed unimplemented functionality — an example.**\n"
            "```\n"
        )

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_tier_declared_at_section_depth_fails(self) -> None:
        result = check_config(
            "# Configuration\n\n"
            "## Committed unimplemented functionality — pools\n\n"
            "No present composition parses a pool.\n"
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("declares a tier", result.stdout)

    def test_descending_tier_order_fails(self) -> None:
        result = check_config(
            "# Configuration\n\n"
            "## Deliveries\n\n"
            "Opening prose.\n\n"
            "### Committed unimplemented functionality — pools\n\n"
            "No present composition parses a pool.\n\n"
            "### The file delivery\n\n"
            "The daemon reads the path per preparation.\n"
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("Tier sections", result.stdout)

    def test_ascending_tier_order_passes(self) -> None:
        result = check_config(
            "# Configuration\n\n"
            "## Deliveries\n\n"
            "Opening prose.\n\n"
            "### The file delivery\n\n"
            "The daemon reads the path per preparation.\n\n"
            "### Committed unimplemented functionality — pools\n\n"
            "No present composition parses a pool.\n\n"
            "### Deferred — headroom\n\n"
            "Routed to open questions.\n"
        )

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_committed_section_without_an_absence_claim_fails(self) -> None:
        result = check_config(
            "# Configuration\n\n"
            "## Deliveries\n\n"
            "Opening prose.\n\n"
            "### Committed unimplemented functionality — pools\n\n"
            "The implementing child owns the pool table.\n"
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("never states that no present surface", result.stdout)

    def test_future_marker_in_an_implemented_section_fails(self) -> None:
        result = check_config(
            "# Configuration\n\n"
            "## Deliveries\n\n"
            "The daemon reads two fields. No present composition parses a "
            "pool.\n"
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("assigns behavior to a future change", result.stdout)

    def test_implementing_child_in_an_implemented_section_fails(self) -> None:
        result = check_config(
            "# Configuration\n\n"
            "## Deliveries\n\n"
            "The daemon reads two fields, and the implementing child adds a "
            "third.\n"
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("implementing child", result.stdout)

    def test_mid_sentence_absence_in_an_implemented_section_passes(self) -> None:
        """A descriptive "no present" is not a tier claim.

        This is the case that decides whether the rule is usable. A page
        legitimately says a call has no present usage axis, which asserts
        nothing about any build; matching it would make the checker cry wolf.
        """
        result = check_config(
            "# Configuration\n\n"
            "## Deliveries\n\n"
            "A historical call with no present usage axis keeps its stored "
            "value.\n"
        )

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_open_edges_may_name_unimplemented_work(self) -> None:
        result = check_config(
            "# Configuration\n\n"
            "## Deliveries\n\n"
            "The daemon reads two fields.\n\n"
            "## Open edges\n\n"
            "- Pool selection is committed unimplemented functionality and no "
            "present composition supplies it.\n"
        )

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_missing_enrolled_page_fails(self) -> None:
        result = run_checker({CONFIG_PAGE: "# Configuration\n"})

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("enrolled page is missing", result.stdout)


if __name__ == "__main__":
    unittest.main()
