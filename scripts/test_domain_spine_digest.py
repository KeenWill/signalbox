#!/usr/bin/env python3
"""Exercise the domain API digest's parser and revision selection."""

import os
import unittest
from pathlib import Path
from unittest.mock import patch

import domain_spine_digest as digest


class DomainSpineDigestTests(unittest.TestCase):
    def test_generic_owner_keeps_qualified_method_name(self) -> None:
        snapshot = """\
pub mod sample
pub struct sample::Service<Generator, Transaction>
impl<Generator, Transaction> sample::Service<Generator, Transaction>
pub fn sample::Service<Generator, Transaction>::execute(&self)
"""

        _, items = digest.parse(snapshot)

        functions = [item.name for item in items if item.category == "function"]
        self.assertEqual(functions, ["sample::Service::execute"])

    def test_generic_trait_impl_resets_nested_function_state(self) -> None:
        snapshot = """\
pub mod sample
pub trait sample::PublicTrait
pub fn sample::PublicTrait::call(&self)
impl<Validate> sample::ToolArgumentValidator for Validate
pub fn sample::ToolArgumentValidator::preauthorization(&self)
pub fn sample::ToolArgumentValidator::validate(&self)
"""

        _, items = digest.parse(snapshot)

        functions = [item.name for item in items if item.category == "function"]
        self.assertEqual(functions, ["sample::PublicTrait::call"])

    def test_event_base_selects_previous_snapshot(self) -> None:
        snapshot = Path("docs/api/sample.txt")
        committed_at_base = "pub mod sample\n"
        current = "pub mod sample\npub struct sample::Added\n"

        with (
            patch.dict(os.environ, {"DOMAIN_SPINE_BASE": "event-base"}, clear=True),
            patch.object(digest, "git_text", return_value=committed_at_base) as git_text,
        ):
            previous = digest.previous_text(snapshot, current)

        self.assertEqual(previous, committed_at_base)
        git_text.assert_called_once_with("event-base", snapshot)


if __name__ == "__main__":
    unittest.main()
