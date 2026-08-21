import importlib.util
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/verify-opencode-compat.py"
SPEC = importlib.util.spec_from_file_location("verify_opencode_compat", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(MODULE)


class OpenCodeCompatibilityScriptTests(unittest.TestCase):
    def test_rfc3339_millis_preserves_the_exact_cutoff(self) -> None:
        self.assertEqual(
            MODULE.rfc3339_millis(1_767_225_600_123),
            "2026-01-01T00:00:00.123Z",
        )


if __name__ == "__main__":
    unittest.main()
