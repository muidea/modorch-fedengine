"""Negative cases for the evidence gate; no Cargo or external state needed."""
from copy import deepcopy
import importlib.util
import json
from pathlib import Path
import shutil
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("checker", ROOT / "scripts/check-library-contracts.py")
checker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(checker)


class ContractGateTests(unittest.TestCase):
    def setUp(self):
        self.mapping = json.loads((ROOT / "tests/contracts/mapping.json").read_text())

    def test_mapping_cannot_name_a_nonexistent_test(self):
        self.mapping["tests"][0]["name"] = "missing_test"
        with self.assertRaisesRegex(ValueError, "missing Rust test"):
            checker.validate_mapping(ROOT, self.mapping)

    def test_duplicate_evidence_is_rejected(self):
        self.mapping["tests"].append(deepcopy(self.mapping["tests"][0]))
        with self.assertRaisesRegex(ValueError, "duplicate mapped test"):
            checker.validate_mapping(ROOT, self.mapping)

    def test_unknown_rules_cannot_claim_coverage(self):
        self.mapping["tests"][0]["rules"] = ["FE-NONEXISTENT-001"]
        with self.assertRaisesRegex(ValueError, "unknown or empty rule"):
            checker.validate_mapping(ROOT, self.mapping)

    def test_passing_counts_ignored_failed_and_other_tests_are_not_evidence(self):
        entries = [{"output": "required_test"}]
        for output in ["test result: ok. 129 passed; 0 failed", "test required_test ... ignored",
                       "test required_test ... FAILED", "test other_test ... ok", ""]:
            with self.subTest(output=output), self.assertRaises(ValueError):
                checker.require_evidence(entries, output)
        checker.require_evidence(entries, "test required_test ... ok\n")

    def test_source_paths_cannot_escape_repository(self):
        self.mapping["tests"][0]["source"] = "../modorch-rs/Cargo.toml"
        with self.assertRaisesRegex(ValueError, "out-of-tree"):
            checker.validate_mapping(ROOT, self.mapping)

    def test_service_artifacts_and_test_adapter_dependencies_are_rejected(self):
        for mutation, error in [("binary", "without service binaries"),
                                ("image", "Dockerfile belongs to magicDataHub"),
                                ("dependency", "must not enter library dependencies")]:
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                paths = [ROOT / "Cargo.toml", *sorted((ROOT / "crates").glob("*/Cargo.toml")),
                         ROOT / "tests/support/host/Cargo.toml"]
                for path in paths:
                    target = root / path.relative_to(ROOT)
                    target.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copyfile(path, target)
                checker.validate_library_layout(root)
                if mutation == "binary":
                    with (root / "Cargo.toml").open("a") as output:
                        output.write('\n[[bin]]\nname="service"\npath="service.rs"\n')
                elif mutation == "image":
                    (root / "Dockerfile").write_text("FROM scratch\n")
                else:
                    path = root / "crates/modorch-fedengine/Cargo.toml"
                    path.write_text(path.read_text().replace(
                        "[dependencies]", "[dependencies]\nmodorch-fedengine-test-host.workspace = true"
                    ))
                with self.assertRaisesRegex(ValueError, error):
                    checker.validate_library_layout(root)


if __name__ == "__main__":
    unittest.main()
