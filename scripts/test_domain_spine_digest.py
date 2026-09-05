#!/usr/bin/env python3
"""Exercise the domain API digest's parser and revision selection."""

import os
import tempfile
import unittest
from contextlib import redirect_stdout
from io import StringIO
from pathlib import Path
from unittest.mock import patch

import domain_spine_digest as digest


class DomainSpineDigestTests(unittest.TestCase):
    def test_public_module_is_delta_item_not_top_level_type(self) -> None:
        baseline = "pub mod sample\n"
        current = baseline + "pub mod sample::empty\n"
        expected = """\
sample
  (root): types=0 traits=0 functions=0
    added: module empty
    removed: none
"""

        with tempfile.TemporaryDirectory() as temporary_directory:
            snapshot = Path(temporary_directory) / "sample.txt"
            snapshot.write_text(current)
            output = StringIO()
            with (
                patch.object(digest, "previous_text", return_value=baseline),
                redirect_stdout(output),
            ):
                digest.render("sample", snapshot)

        self.assertEqual(output.getvalue(), expected)

    def test_exported_macro_is_delta_item_not_top_level_function(self) -> None:
        baseline = "pub mod sample\n"
        current = baseline + "pub macro sample::exported {}\n"
        expected = """\
sample
  (root): types=0 traits=0 functions=0
    added: macro exported
    removed: none
"""

        with tempfile.TemporaryDirectory() as temporary_directory:
            snapshot = Path(temporary_directory) / "sample.txt"
            snapshot.write_text(current)
            output = StringIO()
            with (
                patch.object(digest, "previous_text", return_value=baseline),
                redirect_stdout(output),
            ):
                digest.render("sample", snapshot)

        self.assertEqual(output.getvalue(), expected)

    def test_applied_generic_blanket_implementations_are_not_delta_items(self) -> None:
        baseline = "pub mod sample\npub struct sample::Record\n"
        current = baseline + """\
impl<T, U> core::convert::Into<U> for sample::Record where U: core::convert::From<T>
impl<T> core::any::Any for sample::Record where T: 'static
impl<T> core::borrow::Borrow<T> for sample::Record where T: ?core::marker::Sized
impl<T, U> core::convert::TryInto<U> for sample::Record where U: core::convert::TryFrom<T>
"""
        expected = """\
sample
  (root): types=1 traits=0 functions=0
    added: none
    removed: none
"""

        with tempfile.TemporaryDirectory() as temporary_directory:
            snapshot = Path(temporary_directory) / "sample.txt"
            snapshot.write_text(current)
            output = StringIO()
            with (
                patch.object(digest, "previous_text", return_value=baseline),
                redirect_stdout(output),
            ):
                digest.render("sample", snapshot)

        self.assertEqual(output.getvalue(), expected)

    def test_public_static_is_delta_item(self) -> None:
        baseline = "pub mod sample\n"
        current = baseline + "pub static sample::REGISTRY: usize\n"
        expected = """\
sample
  (root): types=0 traits=0 functions=0
    added: static REGISTRY
    removed: none
"""

        with tempfile.TemporaryDirectory() as temporary_directory:
            snapshot = Path(temporary_directory) / "sample.txt"
            snapshot.write_text(current)
            output = StringIO()
            with (
                patch.object(digest, "previous_text", return_value=baseline),
                redirect_stdout(output),
            ):
                digest.render("sample", snapshot)

        self.assertEqual(output.getvalue(), expected)

    def test_associated_type_is_delta_item_not_top_level_type(self) -> None:
        baseline = "pub mod sample\npub trait sample::Source\n"
        current = baseline + "pub type sample::Source::Output\n"
        expected = """\
sample
  (root): types=0 traits=1 functions=0
    added: associated type Source::Output
    removed: none
"""

        with tempfile.TemporaryDirectory() as temporary_directory:
            snapshot = Path(temporary_directory) / "sample.txt"
            snapshot.write_text(current)
            output = StringIO()
            with (
                patch.object(digest, "previous_text", return_value=baseline),
                redirect_stdout(output),
            ):
                digest.render("sample", snapshot)

        self.assertEqual(output.getvalue(), expected)

    def test_inherent_implementation_bound_change_is_delta_identity(self) -> None:
        shared = "pub mod sample\npub struct sample::Service<Handler>\n"
        baseline = shared + (
            "impl<Handler> sample::Service<Handler> where Handler: sample::OldBound\n"
        )
        current = shared + (
            "impl<Handler> sample::Service<Handler> where Handler: sample::NewBound\n"
        )
        expected = """\
sample
  (root): types=1 traits=0 functions=0
    added: implementation Service (inherent)
    removed: implementation Service (inherent)
"""

        with tempfile.TemporaryDirectory() as temporary_directory:
            snapshot = Path(temporary_directory) / "sample.txt"
            snapshot.write_text(current)
            output = StringIO()
            with (
                patch.object(digest, "previous_text", return_value=baseline),
                redirect_stdout(output),
            ):
                digest.render("sample", snapshot)

        self.assertEqual(output.getvalue(), expected)

    def test_enum_variants_and_public_fields_are_delta_members_not_types(self) -> None:
        baseline = "pub mod sample\n"
        current = """\
pub mod sample
pub enum sample::Status
pub sample::Status::Ready
pub struct sample::Record
pub sample::Record::field: u64
"""
        expected = """\
sample
  (root): types=2 traits=0 functions=0
    added: member Record::field, member Status::Ready, type Record, type Status
    removed: none
"""

        with tempfile.TemporaryDirectory() as temporary_directory:
            snapshot = Path(temporary_directory) / "sample.txt"
            snapshot.write_text(current)
            output = StringIO()
            with (
                patch.object(digest, "previous_text", return_value=baseline),
                redirect_stdout(output),
            ):
                digest.render("sample", snapshot)

        self.assertEqual(output.getvalue(), expected)

    def test_nonblanket_trait_implementation_is_delta_identity(self) -> None:
        baseline = "pub mod sample\npub struct sample::Packet\n"
        current = baseline + "impl external::Pod for sample::Packet\n"
        expected = """\
sample
  (root): types=1 traits=0 functions=0
    added: implementation Packet as external::Pod
    removed: none
"""

        with tempfile.TemporaryDirectory() as temporary_directory:
            snapshot = Path(temporary_directory) / "sample.txt"
            snapshot.write_text(current)
            output = StringIO()
            with (
                patch.object(digest, "previous_text", return_value=baseline),
                redirect_stdout(output),
            ):
                digest.render("sample", snapshot)

        self.assertEqual(output.getvalue(), expected)

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
