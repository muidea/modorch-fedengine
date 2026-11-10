"""The integration preflight must reject identity drift without calling Cargo."""
from copy import deepcopy
import importlib.util
from pathlib import Path
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location(
    "preflight", ROOT / ".agents/skills/modorch-fedengine-integration/scripts/preflight.py"
)
preflight = importlib.util.module_from_spec(spec)
spec.loader.exec_module(preflight)


class PreflightIdentityTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="fedengine-preflight-check-")
        self.addCleanup(self.directory.cleanup)
        root = Path(self.directory.name)
        for name in ["modorch", "modorch-fedengine"]:
            base = root / name
            (base / "src").mkdir(parents=True)
            (base / "Cargo.toml").write_text('[package]\nname="' + name + '"\n')
            (base / "src/lib.rs").write_text("pub fn example() {}\n")
        (root / "modorch-fedengine/README.md").write_text("Example package\n")
        self.facade = root / "modorch-fedengine"
        self.metadata = {
            "packages": [
                {"name": "modorch-fedengine", "version": "0.1.0",
                 "manifest_path": str(self.facade / "Cargo.toml")},
                {"name": "modorch", "version": "0.0.3", "source": "agreed-git-source",
                 "id": "framework", "manifest_path": str(root / "modorch/Cargo.toml")},
            ],
            "resolve": {"nodes": [{"id": "framework", "features": ["runtime"]}]},
        }
        self.snapshot = {
            "skill_revision": "1.0.0", "library_contracts_revision": "1.0.0",
            "library": {"version": "0.1.0", "files": preflight.source_files(self.facade, readme=True)},
            "framework": {"version": "0.0.3", "source": "agreed-git-source",
                          "packages": ["modorch"], "allowed_features": ["runtime"],
                          "files": preflight.source_files(root / "modorch", "modorch/")},
        }

    def test_same_version_with_different_source_is_rejected(self):
        self.metadata["packages"][1]["source"] = "different-source"
        with self.assertRaisesRegex(ValueError, "Source/revision mismatch"):
            preflight.evaluate(self.metadata, self.snapshot)

    def test_duplicate_facades_cannot_hide_behind_equal_versions(self):
        self.metadata["packages"].append(deepcopy(self.metadata["packages"][1]))
        with self.assertRaisesRegex(ValueError, "expected one resolved"):
            preflight.evaluate(self.metadata, self.snapshot)

    def test_facade_source_drift_requires_contract_revalidation(self):
        (self.facade / "src/lib.rs").write_text("pub fn changed() {}\n")
        with self.assertRaisesRegex(ValueError, "Facade source fingerprint mismatch"):
            preflight.evaluate(self.metadata, self.snapshot)

    def test_framework_source_drift_and_unknown_features_are_rejected(self):
        metadata = deepcopy(self.metadata)
        metadata["resolve"]["nodes"][0]["features"].append("unknown-feature")
        with self.assertRaisesRegex(ValueError, "unsupported Modorch features"):
            preflight.evaluate(metadata, self.snapshot)
        (self.facade.parent / "modorch/src/lib.rs").write_text("pub fn changed() {}\n")
        with self.assertRaisesRegex(ValueError, "framework source fingerprint mismatch"):
            preflight.evaluate(self.metadata, self.snapshot)


if __name__ == "__main__":
    unittest.main()
