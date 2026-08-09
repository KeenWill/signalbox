#!/usr/bin/env python3
"""Prove check_availability_projections.py fails on a hole and on a rival owner.

A checker that cannot be shown to fail is the false-confidence trap this
repository documents, and this one guards two properties that a broken version
would report as satisfied: that the machine table is total, and that it is
sole. The tree it guards is clean, so a checker that returned zero
unconditionally would look identical in CI. Each rule therefore gets a failing
case and a passing one.

The cases worth reading closely are the three that decide whether rule 3 is
usable rather than merely strict. A fenced configuration sample spells
`on_pool_exhausted` without projecting anything, a heading names a topic rather
than stating a rule, and `docs/invariants.md` is generated, so a violation there
would be reported in a file no author edits. All three must pass, or the rule
gets satisfied with decorative links and stops meaning anything.

Each case runs the checker as a subprocess against a synthetic `docs` tree in a
temporary working directory, so its root-relative discovery sees only the
fixture.
"""

from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

CHECKER = Path(__file__).resolve().parent / "check_availability_projections.py"

OWNER_PAGE = "docs/spec/credential-availability.md"

ROWS = (
    "selected",
    "contended-wait",
    "exhausted-wait",
    "pre-call fail",
    "post-failure fail",
    "successor",
    "terminal",
)

COLUMNS = (
    "Outcome",
    "Turn phase and attempt disposition",
    "Wake condition",
    "Continuation origin, durable records, locks",
    "Transcript producer and entry",
    "Wire projection",
    "Terminal evidence and cause",
    "Tier and implementing child",
)


def owner_page(
    rows: tuple[str, ...] = ROWS,
    columns: tuple[str, ...] = COLUMNS,
    blank_cell: tuple[int, int] | None = None,
    placeholder: str = "",
) -> str:
    """Render an owner page whose table can be perturbed one axis at a time."""
    lines = [
        "# Credential availability",
        "",
        "## The credential-availability machine",
        "",
        "This build composes no credential pool.",
        "",
        "### Committed unimplemented functionality — the machine",
        "",
        "No present composition resolves a pool.",
        "",
        "| " + " | ".join(columns) + " |",
        "| " + " | ".join("---" for _ in columns) + " |",
    ]
    for row_index, row in enumerate(rows):
        cells = [f"**`{row}`**"]
        for column_index in range(1, len(columns)):
            if blank_cell == (row_index, column_index):
                cells.append(placeholder)
            else:
                cells.append(f"stated for {row}")
        lines.append("| " + " | ".join(cells) + " |")
    lines.append("")
    return "\n".join(lines) + "\n"


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


def check(other: dict[str, str] | None = None, **owner: object) -> (
    subprocess.CompletedProcess[str]
):
    pages = {OWNER_PAGE: owner_page(**owner)}  # type: ignore[arg-type]
    pages.update(other or {})
    return run_checker(pages)


class TableTotalityTests(unittest.TestCase):
    def test_complete_table_passes(self) -> None:
        result = check()

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("7 endings x 7 projections", result.stdout)

    def test_missing_owner_page_fails(self) -> None:
        result = run_checker({"docs/spec/other.md": "# Other\n"})

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("the machine has no owner", result.stdout)

    def test_empty_cell_fails(self) -> None:
        result = check(blank_cell=(1, 2), placeholder="")

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("is empty for ending", result.stdout)

    def test_placeholder_cell_fails(self) -> None:
        """An em dash reads as filled and says nothing, which is the defect."""
        result = check(blank_cell=(3, 5), placeholder="—")

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("is empty for ending", result.stdout)

    def test_renamed_column_fails(self) -> None:
        columns = ("Outcome", "Turn phase") + COLUMNS[2:]
        result = check(columns=columns)

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("not the declared projections", result.stdout)

    def test_dropped_row_fails(self) -> None:
        result = check(rows=ROWS[:-1])

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("not the declared 7", result.stdout)

    def test_reordered_rows_fail(self) -> None:
        rows = (ROWS[1], ROWS[0]) + ROWS[2:]
        result = check(rows=rows)

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("Rows are declared in order", result.stdout)

    def test_missing_table_fails(self) -> None:
        result = run_checker(
            {
                OWNER_PAGE: (
                    "# Credential availability\n\n"
                    "## The credential-availability machine\n\n"
                    "This build composes no credential pool.\n"
                )
            }
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("no machine table found", result.stdout)


class SoleOwnershipTests(unittest.TestCase):
    def test_unlinked_projection_fails(self) -> None:
        result = check(
            {
                "docs/spec/persistence-protocol.md": (
                    "# Persistence\n\n"
                    "A `fail` pool consults `on_pool_exhausted` and stores no "
                    "wait.\n"
                )
            }
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("without linking", result.stdout)

    def test_linked_projection_passes(self) -> None:
        result = check(
            {
                "docs/spec/persistence-protocol.md": (
                    "# Persistence\n\n"
                    "A `fail` pool consults `on_pool_exhausted` as "
                    "[credential availability](credential-availability.md) "
                    "states.\n"
                )
            }
        )

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_projection_inside_a_fence_passes(self) -> None:
        result = check(
            {
                "docs/spec/persistence-protocol.md": (
                    "# Persistence\n\n"
                    "An example document:\n\n"
                    "```toml\n"
                    'on_pool_exhausted = "park"\n'
                    "```\n"
                )
            }
        )

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_projection_in_a_heading_passes(self) -> None:
        result = check(
            {
                "docs/spec/model-call-execution.md": (
                    "# Model-call execution\n\n"
                    "## Availability successor calls\n\n"
                    "The predecessor stays terminal.\n"
                )
            }
        )

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_generated_invariant_index_is_exempt(self) -> None:
        result = check(
            {
                "docs/invariants.md": (
                    "# Invariants\n\n"
                    "A row naming `on_pool_exhausted` with no link.\n"
                )
            }
        )

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_owner_page_does_not_have_to_link_to_itself(self) -> None:
        result = check()

        self.assertEqual(result.returncode, 0, result.stdout)


if __name__ == "__main__":
    unittest.main()
