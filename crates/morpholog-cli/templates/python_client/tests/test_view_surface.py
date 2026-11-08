"""The reader of `generate views --describe` against the committed
trade-lifecycle description, and the catalogue comparison a host runs
before trusting it."""

import json
import unittest

from _support import TEMPLATES_DIR

from python_client import envelopes

DESCRIPTION = (
    TEMPLATES_DIR.parents[2] / "examples" / "10_trade_lifecycle" / "trade_lifecycle_views.json"
)


def surface() -> envelopes.ViewSurface:
    return envelopes.read_view_surface(str(DESCRIPTION))


def catalogue_rows(described: envelopes.ViewSurface) -> list[tuple[str, str, str, str]]:
    return [(described.model_hash, v.predicate, v.view, v.kind) for v in described.views]


class ReadingTheDescription(unittest.TestCase):
    def test_reads_every_view_with_typed_columns(self):
        described = surface()
        self.assertEqual(described.format, 1)
        self.assertEqual(described.programme, "trade_lifecycle")
        self.assertTrue(described.model_hash.startswith("sha256:"))
        self.assertEqual(described.catalog, "_morpholog_catalog")
        kinds = {v.kind for v in described.views}
        self.assertEqual(kinds, {"base", "derived"})
        base = next(v for v in described.views if v.kind == "base")
        self.assertEqual(
            [c.name for c in base.columns[:3]],
            ["_morpholog_asserted_in", "_morpholog_asserted_at", "_morpholog_arguments"],
        )
        self.assertEqual([c.sql_type for c in base.columns[:3]], ["uuid", "timestamptz", "jsonb"])
        fields = [c for c in base.columns if c.role == "field"]
        self.assertEqual([c.position for c in fields], list(range(len(fields))))
        self.assertTrue(all(c.kind for c in fields))

    def test_single_key_is_the_one_single_field_clause_or_none(self):
        one = envelopes.ViewDescription("P", "p", "base", [], [["item"]])
        composite = envelopes.ViewDescription("P", "p", "base", [], [["a", "b"]])
        several = envelopes.ViewDescription("P", "p", "base", [], [["a"], ["b"]])
        none = envelopes.ViewDescription("P", "p", "base", [], [])
        self.assertEqual(one.single_key(), "item")
        self.assertIsNone(composite.single_key())
        self.assertIsNone(several.single_key())
        self.assertIsNone(none.single_key())

    def test_a_drifted_description_is_refused(self):
        payload = json.loads(DESCRIPTION.read_text())
        payload["views"][0]["columns"][0]["nullable"] = True
        with self.assertRaises(envelopes.EnvelopeError):
            envelopes.ViewSurface.from_json(payload)
        payload = json.loads(DESCRIPTION.read_text())
        payload["format"] = 2
        with self.assertRaises(envelopes.EnvelopeError):
            envelopes.ViewSurface.from_json(payload)


class ComparingWithTheCatalogue(unittest.TestCase):
    def test_a_matching_catalogue_has_no_differences(self):
        described = surface()
        self.assertEqual(envelopes.catalogue_differences(described, catalogue_rows(described)), [])

    def test_another_model_hash_and_a_missing_view_are_named(self):
        described = surface()
        rows = catalogue_rows(described)
        other = [("sha256:" + "0" * 64, p, v, k) for _, p, v, k in rows[1:]]
        differences = envelopes.catalogue_differences(described, other)
        self.assertEqual(len(differences), 2, differences)
        self.assertIn("model hash", differences[0])
        self.assertIn(rows[0][2], differences[1])
        extra = rows + [(described.model_hash, "Ghost", "ghost", "base")]
        self.assertEqual(
            envelopes.catalogue_differences(described, extra),
            ["catalogue view ghost (base Ghost) is not described"],
        )
