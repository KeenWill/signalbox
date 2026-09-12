#!/usr/bin/env python3
"""Exercise citation evidence against an immutable Git head."""
from pathlib import Path
import tempfile
import unittest

from review_citations import citations, git, resolve_findings


class CitationResolutionTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.tree = Path(self.temporary.name)
        git(self.tree, 'init', '--initial-branch=main')

    def commit(self, files):
        for path, text in files.items():
            target = self.tree / path
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(text)
        git(self.tree, 'add', '.')
        git(self.tree, '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.com',
            'commit', '-m', 'Record fixture')
        return git(self.tree, 'rev-parse', 'HEAD').decode().strip()

    def test_removed_identifier_is_absent_even_when_the_base_contains_it(self):
        self.commit({'src/lib.rs': 'fn old_gate() {}\n'})
        head = self.commit({'src/lib.rs': 'fn current_gate() {}\n'})
        finding = {'finding_id': 'gate', 'finding_text': 'The `old_gate()` rejects input.'}
        evidence = resolve_findings(self.tree, head, [finding])[0]['citation_resolution']
        self.assertEqual(evidence['head_sha'], head)
        self.assertEqual(evidence['references'], [{
            'citation': {'kind': 'identifier', 'value': 'old_gate'},
            'status': 'absent_at_head', 'source_match_count': 0, 'matches': [],
        }])

    def test_cited_lines_return_current_text_not_worktree_edits(self):
        head = self.commit({'src/lib.rs': 'fn gate() {\n    allow();\n}\n'})
        (self.tree/'src/lib.rs').write_text('fn gate() { deny(); }\n')
        finding = {'finding_id': 'line', 'finding_text': 'See `src/lib.rs:2-3`.'}
        evidence = resolve_findings(self.tree, head, [finding])[0]['citation_resolution']
        self.assertEqual(evidence['references'], [{
            'citation': {'kind': 'path', 'value': 'src/lib.rs', 'line': 2, 'end_line': 3},
            'status': 'found_at_head', 'source_match_count': 1, 'matches': [{
                'path': 'src/lib.rs', 'line': 2, 'end_line': 3,
                'text': '    allow();\n}',
            }],
        }])

    def test_missing_path_and_out_of_range_line_are_absent(self):
        head = self.commit({'src/lib.rs': 'fn gate() {}\n'})
        finding = {'finding_id': 'missing', 'finding_text': 'See `src/gone.rs:1` and `src/lib.rs:9`.'}
        evidence = resolve_findings(self.tree, head, [finding])[0]['citation_resolution']
        self.assertEqual([x['status'] for x in evidence['references']],
                         ['absent_at_head', 'absent_at_head'])

    def test_basename_and_github_line_anchor_resolve_at_the_selected_head(self):
        head = self.commit({'db/migration.sql': 'SELECT 1;\nSELECT 2;\n'})
        finding = {'finding_id': 'paths', 'body':
                   'See `migration.sql:2` and [source](https://github.com/example/repo/blob/old/db/migration.sql#L1-L2).'}
        evidence = resolve_findings(self.tree, head, [finding])[0]['citation_resolution']
        self.assertEqual([x['matches'] for x in evidence['references']], [
            [{'path': 'db/migration.sql', 'line': 2, 'end_line': 2, 'text': 'SELECT 2;'}],
            [{'path': 'db/migration.sql', 'line': 1, 'end_line': 2, 'text': 'SELECT 1;\nSELECT 2;'}],
        ])

    def test_identifier_evidence_prefers_cited_source_and_counts_ambiguity(self):
        head = self.commit({'src/a.rs': 'fn gate() {}\nfn other_gate() {}\n',
                            'src/b.rs': 'fn gate() {}\n'})
        finding = {'finding_id': 'symbol', 'path': 'src/b.rs', 'line': 1,
                   'body': 'The `module::gate()` fails.'}
        evidence = resolve_findings(self.tree, head, [finding])[0]['citation_resolution']
        self.assertEqual(evidence['references'][1]['source_match_count'], 2)
        self.assertEqual(evidence['references'][1]['matches'], [
            {'path': 'src/b.rs', 'line': 1, 'text': 'fn gate() {}'},
        ])

    def test_untracked_text_cannot_make_a_missing_identifier_found(self):
        head = self.commit({'src/lib.rs': 'fn current() {}\n'})
        (self.tree/'extra.rs').write_text('fn missing_symbol() {}\n')
        evidence = resolve_findings(self.tree, head, [{'finding_text': '`missing_symbol`'}])[0]
        self.assertEqual(evidence['citation_resolution']['references'][0]['status'], 'absent_at_head')

    def test_resolution_preserves_the_candidate_and_independent_head_evidence(self):
        head = self.commit({'src/lib.rs': 'fn gate() {}\n'})
        finding = {'finding_id': 'original', 'file_path': 'src/lib.rs',
                   'line_start': '1', 'line_end': '1', 'body': '`gate()`',
                   'is_real_confidence': '9700'}
        result = resolve_findings(self.tree, head, [finding])[0]
        self.assertEqual({k:v for k,v in result.items() if k!='citation_resolution'}, finding)
        self.assertNotIn('citation_resolution', finding)

    def test_mismatched_checkout_is_rejected_before_judgment(self):
        base = self.commit({'src/lib.rs': 'fn earlier() {}\n'})
        self.commit({'src/lib.rs': 'fn current() {}\n'})
        with self.assertRaisesRegex(ValueError, 'differs from the review head'):
            resolve_findings(self.tree, base, [{'finding_text': '`earlier()`'}])

    def test_member_calls_and_bare_code_names_are_citations(self):
        self.assertEqual(citations({'finding_text': '`observation.non_acceptance_proven()` rejects CredentialRejected.'}), [
            {'kind': 'identifier', 'value': 'observation.non_acceptance_proven'},
            {'kind': 'identifier', 'value': 'CredentialRejected'},
        ])


if __name__ == '__main__':
    unittest.main()
