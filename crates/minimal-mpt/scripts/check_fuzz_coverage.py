#!/usr/bin/env python3
from pathlib import Path
import sys


ROOT = Path(__file__).resolve().parents[1]
LCOV = ROOT / "fuzz/coverage/layered_state_ops/lcov.info"


REQUIRED_LINES = {
    "src/state.rs": {
        "commit rollover materializes old intermediate": [163, 164, 166, 167, 169, 170],
        "commit rollover promotes delta and updates padding": [175, 182, 183, 184, 185],
        "delta address-prefix read skip": [199, 201, 202],
        "intermediate address-prefix read skip": [210, 212, 213],
        "delta address-prefix delete skip": [257, 259, 263],
        "intermediate address-prefix delete skip": [271, 274, 278],
        "intermediate tombstone/result handling": [280, 283],
    },
}


def load_lcov(path: Path):
    files = {}
    current = None
    for line in path.read_text().splitlines():
        if line.startswith("SF:"):
            current = line[3:]
            files[current] = {}
        elif line.startswith("DA:") and current is not None:
            number, hits = line[3:].split(",")[:2]
            files[current][int(number)] = int(hits)
    return files


def find_file(files, suffix):
    matches = [name for name in files if name.endswith(suffix)]
    if len(matches) != 1:
        raise AssertionError(f"expected one LCOV file for {suffix}, got {matches}")
    return matches[0]


def main():
    if not LCOV.exists():
        print(f"missing fuzz coverage lcov: {LCOV}", file=sys.stderr)
        return 2

    files = load_lcov(LCOV)
    failures = []
    for suffix, groups in REQUIRED_LINES.items():
        file_name = find_file(files, suffix)
        coverage = files[file_name]
        for group, lines in groups.items():
            missing = [line for line in lines if coverage.get(line, 0) == 0]
            if missing:
                failures.append(f"{suffix}: {group}: missing lines {missing}")

    if failures:
        print("fuzz coverage scope check failed:", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1

    print("fuzz coverage scope check passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
