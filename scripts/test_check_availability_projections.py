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
    cell_text: dict[tuple[int, int], str] | None = None,
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
            override = (cell_text or {}).get((row_index, column_index))
            if blank_cell == (row_index, column_index):
                cells.append(placeholder)
            elif override is not None:
                cells.append(override)
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
        self.assertIn("renders empty for ending", result.stdout)

    def test_placeholder_cell_fails(self) -> None:
        """An em dash reads as filled and says nothing, which is the defect."""
        result = check(blank_cell=(3, 5), placeholder="—")

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("renders empty for ending", result.stdout)

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

    def test_escaped_pipe_in_a_cell_passes(self) -> None:
        """A cell may contain a pipe by escaping it; GFM renders eight columns.

        Splitting on every pipe counted this row as nine cells and failed a
        table that is correct, which is how a gate gets deleted rather than
        fixed.
        """
        result = check(
            cell_text={(1, 2): r"released by `Prepared \| InFlight` completion"}
        )

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_escaped_backslash_leaves_the_next_pipe_a_delimiter(self) -> None:
        """`\\\\` is a literal backslash, so the pipe after it still splits."""
        result = check(cell_text={(1, 2): "ends with a backslash \\\\"})

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_a_stray_unescaped_pipe_still_fails(self) -> None:
        """The escape fix must not stop a real stray pipe being caught.

        GFM truncates a row wider than its header rather than rejecting it, so
        the rendered table still has eight columns and the surplus cell's
        content is silently discarded — worse than an extra column, and absent
        from the parsed tokens, which is why the width comparison is taken
        from the source.
        """
        result = check(cell_text={(1, 2): "one | two"})

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("silently discarded", result.stdout)

    def test_an_escaped_pipe_does_not_hide_an_empty_cell(self) -> None:
        result = check(
            blank_cell=(2, 3),
            placeholder="",
            cell_text={(2, 1): r"phase `a \| b`"},
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("renders empty for ending", result.stdout)

    def test_renamed_outcome_fails(self) -> None:
        """A rename must be caught, not only a reordering.

        Comparing by containment let `not selected` satisfy `selected`, so the
        checker reported a valid seven-ending partition while the declared
        outcome was absent.
        """
        result = check(rows=("not selected",) + ROWS[1:])

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("compared by", result.stdout)

    def test_cell_holding_only_an_html_comment_fails(self) -> None:
        """The guarantee is over the rendering, not the source bytes."""
        result = check(blank_cell=(2, 4), placeholder="<!-- TODO -->")

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("renders empty for ending", result.stdout)

    def test_cell_holding_only_emphasis_fails(self) -> None:
        result = check(blank_cell=(3, 5), placeholder="** **")

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("renders empty for ending", result.stdout)

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

    def test_bare_filename_mention_fails(self) -> None:
        """Naming the file is not linking to it.

        This is the hole worth a fixture of its own: a block asserting that the
        owner page does *not* cover a newly stated outcome contains the
        filename, and a substring test accepted it — so the gate was satisfied
        by exactly the competing, unlinked projection it exists to prevent.
        """
        result = check(
            {
                "docs/spec/persistence-protocol.md": (
                    "# Persistence\n\n"
                    "A `fail` pool consults `on_pool_exhausted`. Note that "
                    "credential-availability.md does not cover this outcome.\n"
                )
            }
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("without a link whose destination resolves", result.stdout)

    def test_filename_inside_a_code_span_fails(self) -> None:
        result = check(
            {
                "docs/spec/persistence-protocol.md": (
                    "# Persistence\n\n"
                    "A `fail` pool consults `on_pool_exhausted`, as "
                    "`credential-availability.md` says.\n"
                )
            }
        )

        self.assertEqual(result.returncode, 1, result.stdout)

    def test_collapsed_reference_link_passes(self) -> None:
        """`[label][]` is a navigable link and must satisfy the rule."""
        result = check(
            {
                "docs/spec/persistence-protocol.md": (
                    "# Persistence\n\n"
                    "A `fail` pool consults `on_pool_exhausted` per "
                    "[the machine][].\n\n"
                    "[the machine]: credential-availability.md\n"
                )
            }
        )

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_definition_inside_a_fence_does_not_satisfy(self) -> None:
        """An example is not a definition, so the shortcut stays unresolved."""
        result = check(
            {
                "docs/spec/persistence-protocol.md": (
                    "# Persistence\n\n"
                    "A `fail` pool consults `on_pool_exhausted` per [machine].\n\n"
                    "```markdown\n"
                    "[machine]: credential-availability.md\n"
                    "```\n"
                )
            }
        )

        self.assertEqual(result.returncode, 1, result.stdout)

    def test_reference_style_link_passes(self) -> None:
        result = check(
            {
                "docs/spec/persistence-protocol.md": (
                    "# Persistence\n\n"
                    "A `fail` pool consults `on_pool_exhausted` as "
                    "[the machine][machine] states.\n\n"
                    "[machine]: credential-availability.md\n"
                )
            }
        )

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_link_from_a_parent_directory_resolves(self) -> None:
        result = check(
            {
                "docs/scenarios.md": (
                    "# Scenarios\n\n"
                    "An `availability successor` is owned by "
                    "[the machine](spec/credential-availability.md).\n"
                )
            }
        )

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_link_to_a_same_named_file_elsewhere_fails(self) -> None:
        """Resolution, not filename matching, decides."""
        result = check(
            {
                "docs/spec/persistence-protocol.md": (
                    "# Persistence\n\n"
                    "A `fail` pool consults `on_pool_exhausted`, see "
                    "[elsewhere](../notes/credential-availability.md).\n"
                ),
                "docs/notes/credential-availability.md": "# Notes\n",
            }
        )

        self.assertEqual(result.returncode, 1, result.stdout)

    def test_fragment_only_link_fails(self) -> None:
        result = check(
            {
                "docs/spec/persistence-protocol.md": (
                    "# Persistence\n\n"
                    "A `fail` pool consults `on_pool_exhausted`, see "
                    "[below](#the-credential-availability-machine).\n"
                )
            }
        )

        self.assertEqual(result.returncode, 1, result.stdout)

    def test_link_with_a_fragment_passes(self) -> None:
        result = check(
            {
                "docs/spec/persistence-protocol.md": (
                    "# Persistence\n\n"
                    "A `fail` pool consults `on_pool_exhausted` as "
                    "[the machine](credential-availability.md#the-credential-availability-machine) "
                    "states.\n"
                )
            }
        )

        self.assertEqual(result.returncode, 0, result.stdout)


if __name__ == "__main__":
    unittest.main()
