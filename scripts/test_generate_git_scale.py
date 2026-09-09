"""Invalid scale sizes fail before creating a repository or invoking Git."""

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
        for blob, tracked, packed in [(1024, 512, 2048), (0, 1024, 2048), (512, 2048, 1024)]:
            with self.subTest(blob=blob, tracked=tracked, packed=packed):
                with tempfile.TemporaryDirectory() as scratch:
                    root = Path(scratch) / 'repository'
                    with patch.object(generator, 'git', side_effect=AssertionError('Git must not run')):
                        with self.assertRaises(ValueError):
                            generator.generate(root, 10, packed, blob, tracked, 2)
                    self.assertFalse(root.exists())
                    self.assertEqual(list(Path(scratch).iterdir()), [])


if __name__ == '__main__':
    unittest.main()
