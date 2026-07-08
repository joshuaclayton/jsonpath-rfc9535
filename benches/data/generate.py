#!/usr/bin/env python3
"""Deterministically generate large bookstore fixtures for the criterion benches.

The fixtures share the schema of the small inline `bookstore()` document in
`benches/queries.rs` (so the same queries apply at every scale) but add many books and
a nested `reviews` array per book, giving descendant/wildcard queries real work.

Usage (regenerate after changing sizes):
    python3 benches/data/generate.py

Deterministic: seeded PRNG, no timestamps, so re-running produces byte-identical files.
"""

import json
import random
import sys
from pathlib import Path

CATEGORIES = ["reference", "fiction", "biography", "technical", "poetry"]
SIZES = {"1k": 1_000, "10k": 10_000, "25k": 25_000, "50k": 50_000, "100k": 100_000}
# Written by default: 1k (small-doc rows), 25k (the iteration workhorse), 100k (the
# at-scale story). 10k/50k are opt-in for occasional full-staircase sweeps — 50k in
# particular sits on this machine's cache cliff and cannot resolve small effects.
DEFAULT_SIZES = ["1k", "25k", "100k"]


def book(rng: random.Random, index: int) -> dict:
    review_count = rng.randint(0, 3)
    return {
        "category": CATEGORIES[index % len(CATEGORIES)],
        "author": f"Author {rng.randint(1, 500)}",
        "title": f"Book Number {index}",
        "isbn": f"{rng.randint(0, 9)}-{rng.randint(100, 999)}-{index:06d}-{rng.randint(0, 9)}",
        "price": round(rng.uniform(1.0, 50.0), 2),
        "in_stock": rng.random() > 0.2,
        "tags": rng.sample(["sale", "new", "rare", "signed", "used"], k=rng.randint(0, 3)),
        "reviews": [
            {"reviewer": f"User {rng.randint(1, 9999)}", "rating": rng.randint(1, 5)}
            for _ in range(review_count)
        ],
    }


def store(count: int) -> dict:
    rng = random.Random(0xC0FFEE ^ count)
    return {
        "store": {
            "book": [book(rng, index) for index in range(count)],
            "bicycle": {"color": "red", "price": 399.0},
        }
    }


def main() -> None:
    requested = sys.argv[1:] or DEFAULT_SIZES
    here = Path(__file__).parent
    for label in requested:
        if label not in SIZES:
            print(f"unknown size {label!r}; choose from {', '.join(SIZES)}")
            continue
        path = here / f"bookstore-{label}.json"
        if path.exists():
            print(f"{path.name}: present, skipping")
            continue
        text = json.dumps(store(SIZES[label]), separators=(",", ":"))
        path.write_text(text + "\n")
        print(f"{path.name}: {SIZES[label]} books, {len(text):,} bytes")


if __name__ == "__main__":
    main()
