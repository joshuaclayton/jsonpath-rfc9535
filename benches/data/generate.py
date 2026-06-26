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
from pathlib import Path

CATEGORIES = ["reference", "fiction", "biography", "technical", "poetry"]
SIZES = {"1k": 1_000, "10k": 10_000}


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
    here = Path(__file__).parent
    for label, count in SIZES.items():
        path = here / f"bookstore-{label}.json"
        text = json.dumps(store(count), separators=(",", ":"))
        path.write_text(text + "\n")
        print(f"{path.name}: {count} books, {len(text):,} bytes")


if __name__ == "__main__":
    main()
