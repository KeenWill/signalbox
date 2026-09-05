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
    def test_negative_auto_trait_implementation_is_not_a_delta_item(self) -> None:
        baseline = "pub mod sample\npub struct sample::Packet\n"
        current = baseline + "impl !core::marker::Freeze for sample::Packet\n"
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

    def test_generic_local_type_parameters_do_not_retain_blanket_impls(self) -> None:
        baseline = "pub mod sample\npub enum sample::Setting<T>\n"
        current = baseline + """\
impl<T, U> core::convert::Into<U> for sample::Setting<T> where U: core::convert::From<T>
impl<T> core::any::Any for sample::Setting<T> where T: 'static
impl<T> core::borrow::Borrow<T> for sample::Setting<T> where T: ?core::marker::Sized
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

    def test_callable_bounds_keep_trait_and_subject_identity(self) -> None:
        baseline = """\
pub mod sample
pub struct sample::Handler<F>
pub struct sample::Output
"""
        current = baseline + """\
impl<F: core::ops::Fn() -> sample::Output, Output> external::Callable<fn() -> Output> for sample::Handler<F>
pub type sample::Handler<F>::Result = Output
"""
        expected = """\
sample
  (root): types=2 traits=0 functions=0
    added: associated type Handler::Result, implementation Handler<F> as external::Callable<fn() -> Output>
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

    def test_local_trait_for_foreign_type_uses_trait_ownership(self) -> None:
        baseline = "pub mod sample\npub trait sample::Visible\n"
        current = baseline + """\
impl sample::Visible for alloc::string::String
pub type sample::Visible::Output
"""
        expected = """\
sample
  (root): types=0 traits=1 functions=0
    added: associated type Visible::Output, implementation alloc::string::String as sample::Visible
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

    def test_wrapped_local_subjects_use_subject_ownership(self) -> None:
        baseline = "pub mod sample\npub struct sample::Packet\n"
        current = baseline + """\
impl<'a> core::fmt::Display for &'a sample::Packet
impl core::fmt::Display for alloc::boxed::Box<sample::Packet>
"""
        expected = """\
sample
  (root): types=1 traits=0 functions=0
    added: implementation &'a sample::Packet as core::fmt::Display, implementation alloc::boxed::Box<sample::Packet> as core::fmt::Display
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

    def test_implementation_identity_keeps_trait_and_subject_arguments(self) -> None:
        shared = """\
pub mod sample
pub struct sample::ArgumentA
pub struct sample::ArgumentB
pub struct sample::Target<T>
"""
        baseline = shared + (
            "impl core::convert::From<sample::ArgumentA> for "
            "sample::Target<sample::ArgumentA>\n"
        )
        current = shared + (
            "impl core::convert::From<sample::ArgumentB> for "
            "sample::Target<sample::ArgumentB>\n"
        )
        expected = """\
sample
  (root): types=3 traits=0 functions=0
    added: implementation Target<sample::ArgumentB> as core::convert::From<sample::ArgumentB>
    removed: implementation Target<sample::ArgumentA> as core::convert::From<sample::ArgumentA>
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

    def test_generic_external_trait_implementation_is_delta_identity(self) -> None:
        baseline = "pub mod sample\npub struct sample::Bag\n"
        current = baseline + (
            "impl<T> core::iter::traits::collect::FromIterator<T> for sample::Bag\n"
        )
        expected = """\
sample
  (root): types=1 traits=0 functions=0
    added: implementation Bag as core::iter::traits::collect::FromIterator<T>
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

    def test_unsafe_trait_implementation_is_delta_identity(self) -> None:
        baseline = "pub mod sample\npub struct sample::Packet\n"
        current = baseline + "unsafe impl external::Pod for sample::Packet\n"
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

    def test_mutable_static_uses_qualified_name_as_delta_identity(self) -> None:
        baseline = "pub mod sample\n"
        current = baseline + "pub static mut sample::REGISTRY: usize\n"
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
impl<T> alloc::string::ToString for sample::Record where T: core::fmt::Display + ?core::marker::Sized
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

    def test_duplicate_declarations_do_not_create_delta_multiplicity(self) -> None:
        shared = "pub mod sample\npub struct sample::Service\n"
        implementation = "impl sample::Service\n"
        baseline = shared + implementation + implementation
        current = shared + implementation
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

    def test_api_snapshots_are_rust_reaching_paths(self) -> None:
        workflow = (
            Path(__file__).resolve().parents[1] / ".github/workflows/rust.yml"
        ).read_text()

        self.assertIn("docs/api/* | docs/domain-spine.md", workflow)

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
    added: implementation Service<Handler> (inherent)
    removed: implementation Service<Handler> (inherent)
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
