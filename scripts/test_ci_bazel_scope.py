"""Regression cases for conservative PostgreSQL dependency selection."""

from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

from ci_bazel_scope import select_suites, source_path
from postgres_integration_suites import Suite


class SuiteScopeTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.persistence = Suite("persistence", "persistence", (), 6, (), (), ())
        self.daemon = Suite("signalboxd", "daemon", (), 1, (), (), ())
        self.client = Suite("terminal-client", "client", (), 1, (), (), ())
        self.suites = (self.persistence, self.daemon, self.client)
        path = self.root / "apps/signalboxd/src/lib.rs"
        path.parent.mkdir(parents=True)
        path.write_text("// source\n")

    def test_daemon_change_includes_its_downstream_client_suite(self):
        with patch("ci_bazel_scope.query_sources", side_effect=[
            {"crates/persistence/src/lib.rs"},
            {"apps/signalboxd/src/lib.rs"},
            {"apps/client/src/lib.rs", "apps/signalboxd/src/lib.rs"},
        ]):
            selected = select_suites(self.suites, ["apps/signalboxd/src/lib.rs"], self.root)
        self.assertEqual(selected, (self.daemon, self.client))

    def test_build_definition_change_keeps_every_suite_without_querying(self):
        with patch("ci_bazel_scope.query_sources") as query:
            selected = select_suites(self.suites, ["crates/persistence/BUILD.bazel"], self.root)
        self.assertEqual(selected, self.suites)
        query.assert_not_called()

    def test_removed_source_keeps_every_suite(self):
        self.assertEqual(select_suites(self.suites, ["apps/client/src/removed.rs"], self.root), self.suites)

    def test_source_unknown_to_the_query_keeps_every_suite(self):
        with patch("ci_bazel_scope.query_sources", return_value=set()):
            selected = select_suites(self.suites, ["apps/signalboxd/src/lib.rs"], self.root)
        self.assertEqual(selected, self.suites)

    def test_missing_path_detail_keeps_every_suite(self):
        self.assertEqual(select_suites(self.suites, [], self.root), self.suites)

    def test_query_failure_does_not_produce_an_empty_matrix(self):
        with patch("ci_bazel_scope.query_sources", side_effect=subprocess.CalledProcessError(1, "bazel")):
            with self.assertRaises(subprocess.CalledProcessError):
                select_suites(self.suites, ["apps/signalboxd/src/lib.rs"], self.root)

    def test_external_source_labels_are_not_local_dependencies(self):
        self.assertIsNone(source_path("@@crates//foo:lib.rs"))
        self.assertEqual(source_path("@@//apps/client:src/lib.rs"), "apps/client/src/lib.rs")


if __name__ == "__main__":
    unittest.main()
