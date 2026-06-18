#!/usr/bin/env python3
from pathlib import Path
import sys


ROOT = Path(__file__).resolve().parents[1]
LCOV = ROOT / "target/coverage/oracle-cfx-storage/lcov.info"


REQUIRED_LINES = {
    "crates/dbs/storage/src/state_manager.rs": {
        "StateIndex height and next-epoch construction": [56, 62, 82, 87, 102, 107],
    },
    "crates/dbs/storage/src/impls/state.rs": {
        "get/set/delete API": [234, 239, 243, 254, 263, 264],
        "prefix API read/write entrypoints": [300, 303, 306, 309],
        "root and commit path": [323, 326, 337, 340, 365, 376],
        "delete_all delta scan/delete": [841, 846, 854],
        "delete_all intermediate scan": [870, 880, 882],
        "delete_all intermediate address-prefix filtering/result/tombstone handling": [
            947,
            955,
            956,
            961,
            965,
        ],
        "delete_all snapshot scan/tombstone handling": [976, 980, 987],
    },
    "crates/dbs/storage/src/impls/state_manager.rs": {
        "next-epoch shift to new delta": [254, 272, 376, 395, 409, 421, 494],
        "next-epoch no-shift path": [503, 549, 561, 573, 591],
        "snapshot creation request": [609, 615],
        "next-epoch public entrypoint": [691, 695, 702, 794, 800, 809],
    },
    "crates/dbs/storage/src/impls/storage_manager/storage_manager.rs": {
        "snapshot background task setup": [542, 552, 574, 594, 607, 621, 640, 649],
        "snapshot registration": [661, 669, 758, 771, 792, 813, 822, 836, 837, 840],
    },
    "crates/dbs/storage/src/impls/merkle_patricia_trie/mpt_merger.rs": {
        "snapshot merge insertion/deletion stream": [73, 82, 88, 90, 91, 114, 115, 116, 117, 119, 120, 121, 125],
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
        print(f"missing oracle coverage lcov: {LCOV}", file=sys.stderr)
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
        print("oracle coverage scope check failed:", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1

    print("oracle coverage scope check passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
