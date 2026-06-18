# Fuzz Coverage Audit

This audit is based only on `cargo +nightly fuzz` corpora and
`cargo +nightly fuzz coverage` output. Unit-test coverage is intentionally not
mixed into these numbers.

## Reports

- Unified layered fuzz coverage is part of the full verification workflow:
  `fuzz/coverage/layered_state_ops/lcov.info`.
- `target/coverage/fuzz-state_ops/summary.txt`
- `target/coverage/fuzz-state_ops/html/index.html`
- `target/coverage/fuzz-layered_state_ops/summary.txt`
- `target/coverage/fuzz-layered_state_ops/html/index.html`
- `target/coverage/fuzz-large_trie_ops/summary.txt`
- `target/coverage/fuzz-large_trie_ops/html/index.html`

## Fuzz Targets

`state_ops` is the delta-only baseline oracle. It mutates one fresh `State`,
checks `get/set/delete/commit`, and exercises the AddressPrefix bug where the
delta prefix is empty.

`layered_state_ops` is the main semantic oracle. It seeds snapshot,
intermediate, and delta state through `PersistedState`, checks layer precedence,
prefix reads/deletes, persisted roundtrip, and the AddressPrefix bug-compatible
delta/intermediate deletion behavior. It now fuzzes `snapshot_epoch_count`
between 1 and 8 and independently models commit height advancement:
intermediate is materialized into snapshot, delta is promoted to intermediate,
and delta key padding is recomputed after rollover. Prefix generation uses
arbitrary prefix bytes and the model mirrors the implementation as raw
delta/intermediate scan followed by canonical AddressPrefix filtering.

`large_trie_ops` is a focused performance-path oracle. It expands small fuzz
inputs into 5000 unique storage keys, then checks reads and commit idempotence.
Its purpose is to cover the parallel trie hashing branch.

## Current Fuzz-Only Coverage

`state_ops`:

- Total lines: 61.94%
- `key_codec.rs`: 74.32%
- `state.rs`: 44.95%
- `trie.rs`: 91.23%

`layered_state_ops`:

- Total lines: 79.92%
- `key_codec.rs`: 92.79%
- `state.rs`: 74.91%
- `trie.rs`: 91.23%
- `store.rs`: 25.64%

`large_trie_ops`:

- Total lines: 35.30%
- `trie.rs`: 95.32%
- `trie.rs` parallel branch covered: `par_iter` lines executed.

## Covered Main Logic

- All seven storage key shapes in Native and Ethereum space.
- Snapshot key encoding and delta/intermediate key encoding.
- `get` precedence: delta, intermediate, snapshot.
- Delta tombstones hiding lower layers.
- `get_all_by_prefix` over delta, intermediate, and snapshot.
- `delete_all_by_prefix` over delta, intermediate, and snapshot.
- Bug-compatible `AddressPrefix`: delta raw prefix is empty.
- Bug-compatible `AddressPrefix` delete: removes all matching raw delta entries
  before canonical filtering.
- Bug-compatible `AddressPrefix` read/delete over intermediate: scans the
  intermediate raw trie first, then canonical-filters nonmatching keys.
- Bug-compatible storage prefix behavior: short storage prefixes use delta-key
  encoding for delta/intermediate scans, so they miss existing full delta storage
  keys while still matching snapshot canonical keys.
- `commit` idempotence and state root construction.
- Latest-only snapshot rollover is covered by unified fuzz and oracle
  comparison with small `snapshot_epoch_count`: delta becomes intermediate,
  prior intermediate is materialized into snapshot, tombstones remove snapshot
  values, and post-rollover reads/prefix operations are checked.
- Persisted-state roundtrip through `State::persisted` and
  `State::from_persisted`.
- Large trie construction and parallel hash path.

## Uncovered Or Low-Covered Areas

Not main MPT logic:

- `FileStore` filesystem read/write and bincode error paths.
- `MemoryStore` and `StateManager` wrapper delegation.
- `Error::Display`, `From<io::Error>`, `From<bincode::ErrorKind>`.
- `H256::Debug` and `H256::zero`.

Covered by unit tests instead of fuzz:

- FileStore latest-state recovery and corrupt-file error.
- MemoryStore/StateManager latest-only persistence.
- Key-codec invalid input rejection.
- `State::from_snapshot`.
- Short test-only account-key compatibility.

Remaining fuzz design gaps:

- `State::from_snapshot` delta-padding-from-root path is not fuzzed because
  the layered oracle currently uses `PersistedState` to seed all three layers.
- Malformed raw delta/snapshot keys are not fuzzed as arbitrary byte streams;
  they are covered by unit tests, not combinational fuzz.
- Store IO is deliberately not fuzzed with the MPT semantic oracle because it
  would mostly exercise filesystem and bincode behavior, not trie semantics.

## Assessment

The important latest-state MPT semantic paths are covered by the unified layered
fuzz state machine or by a focused performance fuzz target. `verify_full.sh`
fails unless `scripts/check_fuzz_coverage.py` sees the required rollover,
AddressPrefix scan/filter, and intermediate tombstone/result lines in
fuzz-generated coverage. The remaining fuzz gaps are either wrapper/IO code or
invalid-input paths already covered by unit tests. The one real semantic gap to
close later is a dedicated fuzz target for `from_snapshot` with root-derived
delta padding.
