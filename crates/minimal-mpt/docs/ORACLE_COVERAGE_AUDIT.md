# Oracle Coverage Audit

This audit is for the upstream `cfx-storage` oracle test tracked on the
`oracle/conflux-rust` submodule branch `minimal-execution-oracle`.
`scripts/run_oracle_compare.sh` now always generates oracle
coverage and runs `scripts/check_oracle_coverage.py`; a plain oracle comparison
without coverage is no longer a supported workflow.

Current report:

- `target/coverage/oracle-cfx-storage/summary.txt`
- `target/coverage/oracle-cfx-storage/lcov.info`
- `target/coverage/oracle-cfx-storage/html/index.html`

## In Scope And Gated

The coverage gate checks representative upstream lines for these semantic
areas. If any are not executed, the workflow fails.

- `StateIndex` height and `new_for_next_epoch` construction.
- `StateTrait` `get`, `set`, `delete`, `read_all`, `delete_all`, root compute,
  and commit paths.
- `delete_all_impl` over delta, intermediate, and snapshot layers.
- Prefix result de-duplication and tombstone creation from lower layers.
- AddressPrefix canonical filtering during delta and intermediate deletion.
- Latest-only rollover through `get_state_for_next_epoch_inner`.
- Snapshot creation request from `State::commit`.
- `StorageManager::check_make_register_snapshot_background`.
- `StorageManager::register_new_snapshot`, including parent delta becoming the
  new snapshot's intermediate MPT.
- Snapshot MPT merge with insertion, deletion, and interleaved insertion/deletion
  streams.

## In Scope Covered By Oracle Assertions

These are checked by explicit trace comparisons, not just line hits.

- Root fields after rollover: snapshot root, intermediate delta root, delta root,
  and state root hash.
- Post-rollover reads from intermediate and then snapshot.
- Tombstone hiding lower-layer values before and after snapshot materialization.
- Storage short-prefix bug: delta/intermediate miss, snapshot hit.
- AddressPrefix bug-compatible delete behavior.
- Intermediate AddressPrefix filtering keeps nonmatching keys while deleting
  matching keys.
- Set order independence for delta root.
- Set/delete/tombstone root behavior.

## Scope Outside This Crate

The following upstream files or paths remain low or zero coverage intentionally.
They are not part of the stand-alone latest-only API.

- `impls/single_mpt_state.rs` and `impls/replicated_state.rs`: migration and
  replicated old/new storage paths.
- `impls/snapshot_sync/**`: snapshot sync, restoration, one-step sync, and slice
  verification.
- `impls/state_proof.rs`, `impls/node_merkle_proof.rs`,
  `impls/proof_merger.rs`, and trie proof code: proof APIs are out of scope.
- `recording_storage.rs`: recording/debug support, not the latest-state API.
- Snapshot pruning, pivot-chain maintenance, extra sync snapshot retention, and
  recovery branches in `storage_manager.rs`.
- RocksDB/sqlite failure, import, cleanup, drop, and recovery branches. This
  crate deliberately keeps persistence separate and does not reproduce upstream
  DB management.
- Delta MPT cache eviction policy internals such as recent-LFU and removable
  heap. They are upstream performance/cache machinery, not semantic oracle
  requirements for this implementation.
- `StateIndex::new_for_readonly` and historical `get_state_no_commit`: version
  lookup and historical reads are out of scope.
- `read_all_with_callback_impl`: the minimal API exposes `get_all_by_prefix`
  returning a vector. The upstream vector path is `read_all` via
  `delete_all_impl<Read>` and is covered.
- `StateManager` branches that wait for missing snapshots, recover a synced
  snapshot, or fall back to `single_mpt`/`replicated_state`: these are upstream
  multi-version/sync/recovery behaviors, not latest-only semantics.
- `StorageManager` snapshot debug checker, recovery-mode registration, DB error
  propagation, and sync-retention flags: these are operational DB/sync paths,
  not part of the stand-alone memory/storage split.
- `MptMerger::merge` single-inserter API remains uncovered; latest-state
  snapshot rollover uses `merge_insertion_deletion_separated`, whose
  insert-only, delete-only, and interleaved paths are covered.

## Known Oracle Limitation

Dirty-state `read_all` on some prefix forms can trigger upstream internal
assertions. The oracle avoids those invalid comparison points and instead
checks the same semantic behavior through stable read/delete paths and
post-rollover states.

## Current Standard

A run is not considered valid unless all of these pass:

```bash
cargo fmt --all -- --check
cargo test -p cfx-minimal-mpt --all-targets
./scripts/run_oracle_compare.sh
```

`run_oracle_compare.sh` includes upstream coverage generation and the scope
coverage gate.
