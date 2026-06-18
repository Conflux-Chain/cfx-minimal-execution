# cfx-minimal-mpt

Standalone minimal MPT implementation for Conflux storage experiments.

This crate is intentionally independent from `conflux-rust`: it does not depend
on `cfx-storage`, `primitives`, `cfx-types`, or the `conflux-rust` workspace.
`conflux-rust` is a workspace-level git submodule used only as an external
oracle for differential testing.

## API

`StateTrait` exposes only:

- `get`
- `set`
- `get_all_by_prefix`
- `delete_all_by_prefix`
- `commit`

`StateManager` manages only the latest state. Historical epoch lookup, proofs,
replication, snapshot sync, and old storage-manager scheduling are out of scope.
Internally, `commit` advances a latest-only block height. Every
`snapshot_epoch_count` commits, the current delta trie is promoted to the
intermediate trie and the previous intermediate trie is materialized into the
snapshot map, matching the old storage latest-state rollover behavior without
keeping historical versions.

## Compatibility Rules

- `StorageKeyWithSpace` is reimplemented locally.
- Snapshot keys use canonical encoding.
- Delta/intermediate trie keys use delta MPT encoding with key padding.
- `get` checks `delta -> intermediate -> snapshot`.
- Empty value in delta/intermediate is a tombstone.
- Prefix APIs follow upstream behavior: ordinary prefixes are encoded with the
  trie-specific key codec; `AddressPrefix` encodes to an empty delta prefix and
  then filters canonical keys, so `delete_all_by_prefix(AddressPrefix)` also
  removes unrelated delta entries.

## Persistence

The pure memory implementation is separated from persistence:

- `MemoryStore` keeps latest committed state in memory.
- `FileStore` writes one latest-state bincode file.

No sqlite dependency is used.

## Tests

Run:

```bash
cargo test -p cfx-minimal-mpt --all-targets
```

Prepare the upstream oracle submodule:

```bash
./scripts/prepare_oracle.sh
```

The oracle lives at `../../oracle/conflux-rust` from this crate. It is not a
workspace member and is not a crate dependency. The oracle comparison test is
tracked on the submodule branch `minimal-execution-oracle`.

Run differential comparison against upstream `cfx-storage`:

```bash
./scripts/run_oracle_compare.sh
```

Generate and review upstream oracle coverage for the injected oracle test:

```bash
./scripts/coverage_oracle_cfx_storage.sh
```

Full verification includes unified layered fuzz coverage and oracle coverage:

```bash
./scripts/verify_full.sh
```

Run fuzzing with nightly:

```bash
cargo +nightly fuzz run state_ops -- -runs=50000
cargo +nightly fuzz run layered_state_ops -- -runs=50000
cargo +nightly fuzz run large_trie_ops -- -runs=100
```

Generate fuzz-only coverage reports:

```bash
./scripts/coverage_fuzz_layered.sh
./scripts/coverage_fuzz_state_ops.sh state_ops
./scripts/coverage_fuzz_state_ops.sh layered_state_ops
./scripts/coverage_fuzz_state_ops.sh large_trie_ops
```

See `docs/FUZZ_COVERAGE_AUDIT.md` for the current fuzz-only coverage assessment
and the remaining non-fuzzed areas.
