"""Regression checks for the cost-shard coverage guard (stdlib only)."""

import importlib.machinery
import importlib.util
from pathlib import Path
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "test-partitions"
LOADER = importlib.machinery.SourceFileLoader("partitions", str(SCRIPT))
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
partitions = importlib.util.module_from_spec(SPEC)
LOADER.exec_module(partitions)


class CoverageTests(unittest.TestCase):
    def setUp(self):
        self.selections = {
            name: {(f"binary-{name}", "test")} for name in partitions.SHARDS
        }
        self.universe = set.union(*self.selections.values())

    def test_disjoint_complete_partition(self):
        partitions.validate_coverage(self.universe, self.selections)

    def test_missing_test(self):
        with self.assertRaisesRegex(ValueError, "missing="):
            partitions.validate_coverage(self.universe | {("new", "test")}, self.selections)

    def test_overlapping_test(self):
        self.selections["cli"] |= self.selections["unit"]
        with self.assertRaisesRegex(ValueError, "overlap="):
            partitions.validate_coverage(self.universe, self.selections)

    def test_extra_test(self):
        self.selections["cli"].add(("extra", "test"))
        with self.assertRaisesRegex(ValueError, "extra="):
            partitions.validate_coverage(self.universe, self.selections)

    def test_empty_shard(self):
        self.selections["heavy"].clear()
        with self.assertRaisesRegex(ValueError, "empty=\\['heavy'\\]"):
            partitions.validate_coverage(self.universe, self.selections)

    def test_empty_inventory(self):
        with self.assertRaisesRegex(ValueError, "empty workspace"):
            partitions.validate_coverage(set(), self.selections)

    def test_wrong_shard_names(self):
        self.selections["typo"] = self.selections.pop("cli")
        with self.assertRaisesRegex(ValueError, "unexpected shard names"):
            partitions.validate_coverage(self.universe, self.selections)

    def test_inventory_keeps_ignored_but_not_filtered_tests(self):
        document = {"rust-suites": {"bin": {"testcases": {
            "ignored": {"ignored": True, "filter-match": {"status": "matches"}},
            "selected": {"ignored": False, "filter-match": {"status": "matches"}},
            "filtered": {"ignored": False, "filter-match": {"status": "mismatch"}},
        }}}}
        self.assertEqual(partitions.selected_tests(document), {
            ("bin", "ignored"), ("bin", "selected"),
        })

    def test_heavy_is_fast_complement_minus_relay_db(self):
        import tomllib
        with (Path(__file__).resolve().parents[2] / ".config/nextest.toml").open("rb") as config:
            fast = tomllib.load(config)["profile"]["fast"]["default-filter"]
        expressions = partitions.filters()
        self.assertEqual(set(expressions), set(partitions.SHARDS))
        for name in ("unit", "daemon", "relay", "cli"):
            self.assertIn(f"({fast}) and (", expressions[name])
        # Relay heavy tests are the relay-db shard; heavy is the remaining
        # fast-filter complement. Together they re-cover the full complement.
        self.assertEqual(expressions["relay-db"], f"(not ({fast})) and ({partitions.RELAY})")
        self.assertEqual(expressions["heavy"], f"(not ({fast})) and not ({partitions.RELAY})")


if __name__ == "__main__":
    unittest.main()
