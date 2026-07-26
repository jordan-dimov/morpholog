"""The envelope models' type annotations, held to the same standard as
their shapes: pinned, not hopeful.

Nothing type-checks this package in CI, so a wrong annotation would
otherwise pass every gate while misleading every reader and IDE. Three
properties close that gap:

1. No container annotation on the consumed surface stays bare, at ANY
   depth - `list[dict]` fails as surely as `list`, because a half-named
   element type is the same defect one level down.
2. The annotations are TRUE of the pinned payloads: each golden is
   parsed and its runtime values checked against what the annotations
   claim. The rule the annotations follow is that they describe what
   the PARSER guarantees, not what today's goldens happen to hold - so
   a `list[str]` field must be parsed through `_str_list`, not merely
   copied out of the payload.
3. The corpus is accounted for BY NAME. More than one class may accept
   a given payload (an empty collection makes two shapes coincide),
   which is harmless - every accepting class gets checked. What would
   not be harmless is a golden quietly participating in nothing, so
   every file is either matched or listed below with its reason.

The check would also be vacuous if every collection it saw were empty,
so it counts the non-empty ones and holds that count to a floor.
"""

import dataclasses
import inspect
import typing
import unittest

from _support import GOLDEN_DIR, add_client_to_path, golden

add_client_to_path()

from python_client import adapter, envelopes  # noqa: E402

# `object` is the honest element type where a decoded value is
# genuinely heterogeneous (a claim's arguments carry decimals, dates,
# subjects); a bare `list` is not.
BARE = {list, dict, set, tuple, frozenset}

# Goldens no single dataclass parses, each for a structural reason:
# they are not one envelope object. The adapter reaches them through a
# list comprehension or by unwrapping a key, so a per-class match is
# not the shape to expect here.
NOT_A_SINGLE_ENVELOPE = {
    "claim_instances.json": "a JSON array of claims; the adapter maps over it",
    "named_claims.json": "a JSON array of named claims",
    "outbox_claim.json": "wrapped as {'row': ...}",
    "outbox_claim_null.json": "wrapped as {'row': null}",
    "batch_score.json": "a score report keyed by case, not one envelope",
    "score_report.json": "a score report, read through its own accessor",
    "score_report_split.json": "a split score report",
}

# The element-type check is only as strong as the payloads it sees:
# an empty list conforms to every `list[X]`. This floor is a measured
# property of the corpus, not an aspiration - it drops if goldens stop
# carrying populated collections.
MIN_NON_EMPTY_COLLECTIONS = 40


def envelope_dataclasses():
    return [
        obj
        for _, obj in inspect.getmembers(envelopes, inspect.isclass)
        if dataclasses.is_dataclass(obj) and obj.__module__ == envelopes.__name__
    ]


def contains_bare_container(annotation: object) -> bool:
    """A bare container ANYWHERE in the annotation tree - `list[dict]`
    is as unfinished as `list`."""
    # `Callable[[X], Y]` hands back its parameter list as an actual list
    # instance, which is unhashable - so membership is by identity, which
    # is what "is this the bare builtin?" means anyway.
    if any(annotation is bare for bare in BARE):
        return True
    return any(contains_bare_container(arg) for arg in typing.get_args(annotation))


def element_types(annotation):
    """The `X` of a `list[X]` (or the value type of a `dict[K, V]`);
    empty when the annotation carries no element type worth checking."""
    origin = typing.get_origin(annotation)
    if origin in (list, set, frozenset):
        return list(typing.get_args(annotation))
    if origin is dict:
        args = typing.get_args(annotation)
        return [args[1]] if len(args) == 2 else []
    return []


def matches(value, annotation) -> bool:
    """Runtime conformance, deliberately permissive about what cannot be
    cheaply decided (Callables, TypeVars): the point is to catch a wrong
    element type, not to reimplement mypy."""
    if annotation is object or isinstance(annotation, typing.TypeVar):
        return True
    origin = typing.get_origin(annotation)
    if origin is typing.Union or type(annotation).__name__ == "UnionType":
        return any(matches(value, a) for a in typing.get_args(annotation))
    if origin in (list, set, frozenset, dict, tuple):
        return isinstance(value, origin)
    if annotation is type(None):
        return value is None
    if isinstance(annotation, type):
        return isinstance(value, annotation)
    return True


class TestNoBareContainers(unittest.TestCase):
    """Property 1: the class stays closed, at every depth."""

    def test_no_envelope_field_is_a_bare_container(self):
        offenders = [
            f"{cls.__name__}.{f.name}"
            for cls in envelope_dataclasses()
            for f in dataclasses.fields(cls)
            if contains_bare_container(typing.get_type_hints(cls).get(f.name))
        ]
        self.assertEqual(
            offenders,
            [],
            "these envelope fields carry a bare container annotation (at some "
            "depth); name the element type - use `object` where values are "
            "genuinely mixed",
        )

    def test_no_client_signature_carries_a_bare_container(self):
        offenders = []
        for module in (envelopes, adapter):
            for name, obj in inspect.getmembers(module):
                if inspect.isfunction(obj) and obj.__module__ == module.__name__:
                    candidates = [(name, obj)]
                elif inspect.isclass(obj) and obj.__module__ == module.__name__:
                    candidates = [
                        (f"{name}.{m}", fn)
                        for m, fn in inspect.getmembers(obj, inspect.isfunction)
                        if fn.__module__ == module.__name__
                    ]
                else:
                    continue
                for label, fn in candidates:
                    # Deliberately unguarded: an annotation that cannot be
                    # resolved is itself a defect, and swallowing the failure
                    # would let this gate go quiet.
                    for param, hint in typing.get_type_hints(fn).items():
                        if contains_bare_container(hint):
                            offenders.append(f"{module.__name__}.{label}({param})")
        self.assertEqual(
            offenders,
            [],
            "these client signatures carry a bare container annotation (at some "
            "depth); name the element type - use `object` where values are "
            "genuinely mixed",
        )


class TestAnnotationsHoldForGoldens(unittest.TestCase):
    """Properties 2 and 3: true of the payloads, and the corpus is
    accounted for by name."""

    def setUp(self):
        self.non_empty = 0

    def check(self, obj, path: str):
        hints = typing.get_type_hints(type(obj))
        for f in dataclasses.fields(obj):
            value = getattr(obj, f.name)
            annotation = hints[f.name]
            where = f"{path}.{f.name}"
            self.assertTrue(
                matches(value, annotation),
                f"{where}: {value!r} does not match {annotation}",
            )
            for element_type in element_types(annotation):
                items = list(value.values() if isinstance(value, dict) else value or [])
                if items:
                    self.non_empty += 1
                for i, item in enumerate(items):
                    self.assertTrue(
                        matches(item, element_type),
                        f"{where}[{i}]: {item!r} does not match {element_type}",
                    )
                    if dataclasses.is_dataclass(item):
                        self.check(item, f"{where}[{i}]")
            if dataclasses.is_dataclass(value):
                self.check(value, where)

    def test_every_golden_is_accounted_for_and_conforms(self):
        goldens = {p.name for p in GOLDEN_DIR.glob("*.json")}
        self.assertGreater(len(goldens), 40, "the golden corpus should not shrink")
        matched = set()
        for name in sorted(goldens):
            payload = golden(name)
            for cls in envelope_dataclasses():
                from_json = getattr(cls, "from_json", None)
                if from_json is None:
                    continue
                try:
                    obj = from_json(payload)
                except Exception:
                    continue  # this class is not this payload's shape
                matched.add(name)
                self.check(obj, f"{name}:{cls.__name__}")

        self.assertEqual(
            sorted(goldens - matched),
            sorted(NOT_A_SINGLE_ENVELOPE),
            "a golden stopped participating in the annotation check (or a newly "
            "unmatched one appeared): every file is either parsed by some "
            "envelope class or listed in NOT_A_SINGLE_ENVELOPE with its reason",
        )
        self.assertGreaterEqual(
            self.non_empty,
            MIN_NON_EMPTY_COLLECTIONS,
            "the element-type check saw too few POPULATED collections to prove "
            "anything - an empty list conforms to every list[X], so this floor "
            "is what keeps property 2 from going vacuous",
        )


if __name__ == "__main__":
    unittest.main()
