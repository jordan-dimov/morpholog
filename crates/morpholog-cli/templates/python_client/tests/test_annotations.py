"""The envelope models' type annotations, held to the same standard as
their shapes: pinned, not hopeful.

Nothing type-checks this package in CI, so a wrong annotation would
otherwise pass every gate while misleading every reader and IDE. Two
properties close that gap:

1. No container annotation on the consumed surface stays bare - a new
   field or method added as `list` (rather than `list[Row]`) reddens
   here, so the class cannot silently reopen.
2. The annotations are TRUE of the real payloads: every golden envelope
   is parsed and its runtime values checked against what the
   annotations claim. `_strict` refuses unknown and missing keys, so
   trying each class against each golden is self-discriminating - a
   golden only parses as the class it belongs to.
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


def envelope_dataclasses():
    return [
        obj
        for _, obj in inspect.getmembers(envelopes, inspect.isclass)
        if dataclasses.is_dataclass(obj) and obj.__module__ == envelopes.__name__
    ]


def element_types(annotation):
    """The `X` of a `list[X]` (or the value type of a `dict[K, V]`),
    unwrapped through unions; empty when the annotation carries no
    element type worth checking."""
    origin = typing.get_origin(annotation)
    if origin in (list, set, frozenset):
        return [a for a in typing.get_args(annotation)]
    if origin is dict:
        args = typing.get_args(annotation)
        return [args[1]] if len(args) == 2 else []
    return []


def matches(value, annotation) -> bool:
    """Runtime conformance, one level deep and deliberately permissive
    about what we cannot cheaply decide (Callables, TypeVars): the
    point is to catch a wrong element type, not to reimplement mypy."""
    if annotation is object or isinstance(annotation, typing.TypeVar):
        return True
    origin = typing.get_origin(annotation)
    if origin is typing.Union or str(origin) == "typing.Union":
        return any(matches(value, a) for a in typing.get_args(annotation))
    # `X | None` under PEP 604 reports its own origin kind.
    if origin is not None and str(origin) in ("<class 'types.UnionType'>",):
        return any(matches(value, a) for a in typing.get_args(annotation))
    if origin in (list, set, frozenset, dict, tuple):
        return isinstance(value, origin)
    if annotation is type(None):
        return value is None
    if isinstance(annotation, type):
        return isinstance(value, annotation)
    return True


class TestNoBareContainers(unittest.TestCase):
    """Property 1: the class stays closed."""

    def test_no_envelope_field_is_a_bare_container(self):
        offenders = []
        for cls in envelope_dataclasses():
            hints = typing.get_type_hints(cls)
            for f in dataclasses.fields(cls):
                if hints.get(f.name) in BARE:
                    offenders.append(f"{cls.__name__}.{f.name}")
        self.assertEqual(
            offenders,
            [],
            "these envelope fields carry a bare container annotation; name the "
            "element type (use `object` where values are genuinely mixed)",
        )

    def test_no_client_signature_returns_a_bare_container(self):
        offenders = []
        for module in (envelopes, adapter):
            for name, obj in inspect.getmembers(module):
                candidates = []
                if inspect.isfunction(obj) and obj.__module__ == module.__name__:
                    candidates = [(name, obj)]
                elif inspect.isclass(obj) and obj.__module__ == module.__name__:
                    candidates = [
                        (f"{name}.{m}", fn)
                        for m, fn in inspect.getmembers(obj, inspect.isfunction)
                        if fn.__module__ == module.__name__
                    ]
                for label, fn in candidates:
                    try:
                        hints = typing.get_type_hints(fn)
                    except Exception:  # pragma: no cover - defensive
                        continue
                    for param, hint in hints.items():
                        if hint in BARE:
                            offenders.append(f"{module.__name__}.{label}({param})")
        self.assertEqual(
            offenders,
            [],
            "these client signatures carry a bare container annotation; name "
            "the element type (use `object` where values are genuinely mixed)",
        )


class TestAnnotationsHoldForGoldens(unittest.TestCase):
    """Property 2: the annotations are true of the pinned payloads."""

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
                items = value.values() if isinstance(value, dict) else value or []
                for i, item in enumerate(items):
                    self.assertTrue(
                        matches(item, element_type),
                        f"{where}[{i}]: {item!r} does not match {element_type}",
                    )
                    if dataclasses.is_dataclass(item):
                        self.check(item, f"{where}[{i}]")
            if dataclasses.is_dataclass(value):
                self.check(value, where)

    def test_every_golden_conforms_to_its_annotations(self):
        goldens = sorted(p.name for p in GOLDEN_DIR.glob("*.json"))
        self.assertGreater(len(goldens), 20, "the golden corpus should not shrink")
        parsed_any = 0
        for name in goldens:
            payload = golden(name)
            for cls in envelope_dataclasses():
                from_json = getattr(cls, "from_json", None)
                if from_json is None:
                    continue
                try:
                    obj = from_json(payload)
                except Exception:
                    continue  # a golden only parses as the class it belongs to
                parsed_any += 1
                self.check(obj, f"{name}:{cls.__name__}")
        self.assertGreater(
            parsed_any, 20, "the annotation check parsed almost no goldens - the "
            "self-discriminating match broke, and this test stopped testing"
        )


if __name__ == "__main__":
    unittest.main()
