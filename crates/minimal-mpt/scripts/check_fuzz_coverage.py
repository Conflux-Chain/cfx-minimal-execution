#!/usr/bin/env python3
from pathlib import Path
import sys


ROOT = Path(__file__).resolve().parents[1]
LCOV = ROOT / "fuzz/coverage/layered_state_ops/lcov.info"


# State was split out of the former monolithic `src/state.rs`: the boundary
# rotation moved to `src/state/rotation.rs` and the prefix read/delete paths to
# `src/state/prefix.rs`. The line groups below track the same semantic branches
# in their new homes; each picks lines on the branch-taken side (e.g. the
# `continue` skips, the tombstone `remove`) so the gate keeps proving the corpus
# exercises those paths rather than merely entering the function.
REQUIRED_LINES = {
    "src/state/rotation.rs": {
        # absorb intermediate into snapshot: both Some(insert) and Tombstone(remove)
        "commit rollover materializes old intermediate": [38, 39, 41, 42],
        # rotate delta -> intermediate and re-derive the delta padding
        "commit rollover promotes delta and updates padding": [63, 65, 66, 67, 73],
    },
    "src/state/prefix.rs": {
        "delta address-prefix read skip": [29, 30],
        "intermediate address-prefix read skip": [40, 41],
        "delta address-prefix delete skip": [90, 92, 94],
        "intermediate address-prefix delete skip": [105, 107, 109],
        "intermediate tombstone/result handling": [111, 112, 114, 115],
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
