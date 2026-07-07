#!/usr/bin/env python3
"""Line-coverage scope gates. Usage: check_coverage.py {fuzz|oracle}

Deliberately line-number-pinned: each group lists lines on the branch-taken
side of a semantic branch, so the gate proves the corpus/test exercises the
path rather than merely entering the function. The pins are brittle across
refactors — only scripts/verify.sh's coverage tier runs them.
"""
from pathlib import Path
import sys


ROOT = Path(__file__).resolve().parents[1]

# Re-pinned 2026-07-07 after the state refactor moved snapshot.rs to
# src/state/layers/. Each group's lines were cross-checked against a fresh
# layered_state_ops lcov (all hit) and include the branch-taken side (the
# `continue` skips, the tombstone arm/remove) so the gate keeps proving the
# corpus exercises those paths rather than merely entering the function.
FUZZ_REQUIRED = {
    "src/state/layers/snapshot.rs": {
        # apply_updates owns the insert/remove branch split used by commit
        # rollover; 75/76 are the Some/Tombstone arms.
        "snapshot batch update insert/remove": [73, 74, 75, 76],
    },
    "src/state/rotation.rs": {
        # absorb intermediate into snapshot before re-rooting (33/34 = loop
        # body, i.e. a non-empty intermediate was materialized)
        "commit rollover materializes old intermediate": [32, 33, 34, 36, 39],
        # rotate delta -> intermediate, re-derive the delta padding, rotate caches
        "commit rollover promotes delta and updates padding": [55, 57, 58, 59, 65],
    },
    "src/state/prefix.rs": {
        "delta address-prefix read skip": [30, 31],
        "intermediate address-prefix read skip": [41, 42],
        "delta address-prefix delete skip": [90, 91, 95],
        "intermediate address-prefix delete skip": [105, 106, 110],
        "intermediate tombstone/result handling": [112, 113, 115, 116, 117],
    },
}

# Line numbers pin sources in the oracle/conflux-rust submodule on the
# `minimal-execution-oracle` branch; re-pin whenever that branch moves.
ORACLE_REQUIRED = {
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

GATES = {
    "fuzz": {
        "lcov": ROOT / "target/coverage/fuzz-layered_state_ops/lcov.info",
        "required": FUZZ_REQUIRED,
    },
    "oracle": {
        "lcov": ROOT / "target/coverage/oracle-cfx-storage/lcov.info",
        "required": ORACLE_REQUIRED,
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
    if len(sys.argv) != 2 or sys.argv[1] not in GATES:
        print(f"usage: {sys.argv[0]} {{fuzz|oracle}}", file=sys.stderr)
        return 2
    kind = sys.argv[1]
    lcov = GATES[kind]["lcov"]

    if not lcov.exists():
        print(f"missing {kind} coverage lcov: {lcov}", file=sys.stderr)
        return 2

    files = load_lcov(lcov)
    failures = []
    for suffix, groups in GATES[kind]["required"].items():
        file_name = find_file(files, suffix)
        coverage = files[file_name]
        for group, lines in groups.items():
            missing = [line for line in lines if coverage.get(line, 0) == 0]
            if missing:
                failures.append(f"{suffix}: {group}: missing lines {missing}")

    if failures:
        print(f"{kind} coverage scope check failed:", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1

    print(f"{kind} coverage scope check passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
