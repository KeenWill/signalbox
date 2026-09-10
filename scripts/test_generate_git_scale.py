"""Scale generation validates sizes and leaves unrelated scratch neighbors intact."""

import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

SOURCE = Path(__file__).resolve().parents[1] / 'tooling/generate-git-scale.py'
SPEC = importlib.util.spec_from_file_location('generate_git_scale', SOURCE)
generator = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(generator)


class ScaleArguments(unittest.TestCase):
    def test_inconsistent_sizes_leave_no_repository(self):
        for objects, blob, tracked, packed in [
            (10, 1024, 512, 2048),
            (10, 0, 1024, 2048),
            (10, 512, 2048, 1024),
            (2, 16, 16, 16),
            (2, 15, 31, 31),
            (2, 32, 40, 40),
            (2, 32, 64, 96),
            (3, 32, 64, 65),
        ]:
            with self.subTest(objects=objects, blob=blob, tracked=tracked, packed=packed):
                with tempfile.TemporaryDirectory() as scratch:
                    root = Path(scratch) / 'repository'
                    with patch.object(generator, 'git', side_effect=AssertionError('Git must not run')):
                        with self.assertRaises(ValueError):
                            generator.generate(root, objects, packed, blob, tracked, 2)
                    self.assertFalse(root.exists())
                    self.assertEqual(list(Path(scratch).iterdir()), [])


    def test_generation_preserves_sibling_files_and_writes_valid_suffix_boundary_blobs(self):
        # Exercise the minimum suffix and a stored-block boundary with a one-byte tail.
        for blob in [16, 65536]:
            with self.subTest(blob=blob), tempfile.TemporaryDirectory() as scratch:
                parent = Path(scratch)
                root = parent / 'repository'
                neighbors = [parent / 'repository-index-records', parent / 'repository-index-records.sorted']
                for neighbor in neighbors:
                    neighbor.write_bytes(b'unrelated content')
                generator.generate(root, 2, blob + 16, blob, blob + 16, 2)
                generator.git(root, 'fsck', '--full', '--no-dangling')
                self.assertEqual(len(generator.git(root, 'cat-file', 'blob', 'HEAD:large.bin')), blob)
                self.assertEqual(len(generator.git(root, 'cat-file', 'blob', 'HEAD:second.bin')), 16)
                for neighbor in neighbors:
                    self.assertEqual(neighbor.read_bytes(), b'unrelated content')
                self.assertEqual(set(parent.iterdir()), {root, *neighbors})


if __name__ == '__main__':
    unittest.main()
