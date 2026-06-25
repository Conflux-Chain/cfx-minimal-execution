# cfx-minimal-execution

Conflux 的完整节点把共识、同步、网络、存储、执行耦合在一起。当你只关心执行层的正确性——验证一套新的 MPT 实现、测试 EVM 修改的回归——启动完整节点既慢又重。本项目把执行层从完整节点中剥离出来：先离线提取历史数据，再脱离节点独立重执行，逐 epoch 校验状态根。

工作流是一条单向 pipeline：

1. **提取**：`cfx-replay-data-extractor` 从全节点的 RocksDB 中读取区块头、交易、收据、奖励，编码为紧凑的 `.cfxpack` 容器文件。这一步需要访问全节点的 `blockchain_data` 目录，完成后不再依赖节点。
2. **重放**：`cfx-replay-exec` 顺序读取容器，驱动 Conflux EVM 逐 epoch 重执行，每个 epoch 产出组合状态根。
3. **验证**：产出的状态根与链上 deferred state root 做前缀对比（4 字节）。运行结束时输出 `state_match=N/N longest_mismatch_run=0` 即全部通过。

## 快速开始

环境要求：Rust stable。提取阶段需要全节点的 `blockchain_data` 目录；重放阶段只需要已提取的 `.cfxpack` 文件和链配置。

```bash
git submodule update --init
```

**构建提取器**（顶层 workspace member，直接构建）：

```bash
cargo build --release -p cfx-replay-data-extractor
```

**构建执行器**（依赖 oracle workspace 内的 EVM 和共识参数 crate，必须在 oracle 路径下编译）：

```bash
cd oracle/conflux-rust/crates/replay-data-executor
cargo build --release --no-default-features --features backend-minimal-mpt
```

**提取数据**：

```bash
cfx-replay-data-extractor extract-packed \
    --data-dir /path/to/blockchain_data \
    --output-dir ./data/packed-full \
    --start-epoch 1 --epoch-count 150000000 \
    --shard-epochs 2000 --jobs 24 \
    --target-bytes 104857600 --prefix epochs \
    --config crates/replay-data-extractor/mainnet.toml
```

| 参数 | 说明 |
|------|------|
| `--start-epoch` | 起始 epoch（含） |
| `--epoch-count` | 提取的 epoch 数量 |
| `--shard-epochs` | 每个 group 的 epoch 数（固定 2000） |
| `--jobs` | 并行提取线程数（建议 CPU 核数） |
| `--target-bytes` | 单个容器文件的目标大小（默认 100 MiB） |
| `--prefix` | 输出文件名前缀 |
| `--config` | 链参数配置（含 `pos_reference_enable_height` 等 CIP 激活高度） |

产出的 `.cfxpack` 文件按 epoch 范围命名（如 `epochs_1_574000.cfxpack`），每个文件约 100 MiB，内部索引支持任意 2000-epoch 组的随机定位。全量提取（~1.5 亿 epoch，24 jobs）约需一天。

> **注意**：`--config` 必须传入正确的链参数。PoS 激活高度（mainnet 37400000）之后的区块携带 `pos_view` 字段，需要配置文件中的 `pos_reference_enable_height` 来正确编解码。Pre-PoS 区间不受影响（新旧 encoder 输出逐字节一致）。

**重放与验证**：

```bash
cfx-replay-exec \
    --input ./data/packed-full \
    --config oracle/conflux-rust/run/hydra.toml \
    --checkpoint ./ckpt.bin \
    --checkpoint-every-groups 100 \
    --checkpoint-every-seconds 600
```

如果 `--checkpoint` 指定的文件已存在，自动从中恢复继续；不存在则从创世块开始。checkpoint 在 2000-epoch 组边界写入，触发条件是"经过 N 个组"或"经过 T 秒"任一先到。如果只想快进到某个高度然后停下，加 `--stop-after-checkpoint`，写完第一次 checkpoint 即退出。

运行过程中每分钟输出进度：

```
replay progress t=2562s height=23694000 groups=... state_match=4580000/4580000 longest_mismatch_run=0
```

结束时会打印验证摘要。`longest_mismatch_run` 持续大于 0 说明存在实现分歧，孤立的 1-2 个 mismatch 通常是链上被 blame 的区块（其 deferred root 被网络拒绝，正确的重放本应与之不同）。

## 运行参数

### 执行器 `cfx-replay-exec`

| 参数 | 默认 | 说明 |
|------|------|------|
| `--input` | 必填 | `.cfxpack` 文件或目录 |
| `--config` | 必填 | 链配置（如 `hydra.toml`） |
| `--checkpoint` | — | checkpoint 路径；存在则 resume |
| `--checkpoint-every-groups` | 100 | 每 N×2000 epoch 写 checkpoint |
| `--checkpoint-every-seconds` | — | 距上次写入超过 T 秒也触发 |
| `--stop-after-checkpoint` | false | 写完第一次 checkpoint 后退出 |
| `--anomaly-streak` | 20 | 连续 mismatch 达到此数中止运行 |
| `--max-mismatches` | 20 | 结束时打印的 mismatch 详情上限 |
| `--verbose-epochs` | false | 逐 epoch 打印校验结果 |

### 提取器 `cfx-replay-data-extractor`

主要入口是 `extract-packed`，其余子命令用于调试和数据维护：

| 子命令 | 用途 |
|--------|------|
| `extract-packed` | 批量提取并打包为 `.cfxpack`（生产入口） |
| `extract-shards` | 按 epoch 分片提取（中间格式，供调试） |
| `extract` | 提取单个 packet（调试） |
| `verify` | 校验 packet 编码完整性 |
| `roundtrip` | 解码再编码，验证可逆性 |
| `bench-decode` | 解码吞吐基准 |
| `add-total-reward-flag` | 补丁工具：为旧格式数据补充 reward=0 标记 |

### Feature flags

执行器通过 feature 选择状态后端，两者互斥：

- `backend-minimal-mpt`：使用本项目的自研 MPT 后端，支持 checkpoint。验证新实现正确性时用这个。
- `backend-cfx-storage`（默认）：使用 Conflux 原始存储层，作为正确性 oracle。

附加 feature：

- `verify-incremental`：每次 `IncrementalTrie::root()` 之后用同一份数据从零全量计算一次，assert 二者相等。用于排查增量缓存/剪枝 bug，速度约降 2×。
- `profile`：启用 pprof-rs 997Hz 采样，进程退出时写 `flamegraph.svg` 和 `profile.folded`。

### 环境变量

| 变量 | 说明 |
|------|------|
| `MMPT_MERGE_TIMING=1` | 打印每次 snapshot 同步 merge 的耗时分解 |
| `MMPT_DELTA_TIMING=1` | 每 20k commit 打印 delta root 平均耗时 |

## 架构与关键设计

本节面向需要修改代码的开发者。如果只是使用，上面的内容已经够了。

### 项目结构

```
crates/
  minimal-mpt/          自研 MPT 后端（本项目核心产出）
  replay-data-extractor/  提取器（顶层 workspace member）
  replay-data-executor/   执行器源码（开发和阅读在此）
oracle/conflux-rust/
  crates/replay-data-executor/  执行器的编译入口（源码与上面同步镜像）
  run/hydra.toml                主网链配置
data/packed-full/       .cfxpack 容器目录
```

执行器源码出现在两个路径下，原因是它依赖 oracle workspace 内的 EVM、共识参数等 crate，Cargo 要求在同一 workspace 内才能解析这些依赖。日常开发在 `crates/replay-data-executor/` 下阅读和修改，构建时进入 `oracle/conflux-rust/crates/replay-data-executor/` 执行 cargo build。两份源码保持同步。

### minimal-mpt：三段式状态与增量根

Conflux 的世界状态不是一棵 trie，而是三棵：snapshot、intermediate、delta。组合状态根 = `keccak(snapshot_root ‖ intermediate_root ‖ delta_root)`。每 2000 epoch 为一个 period，在 period 边界做轮转：snapshot 吸收 intermediate，intermediate 接管上一轮 delta，delta 清空重建。

`minimal-mpt` 用一个 `IncrementalTrie`（BTreeMap + prefix hash cache）实现每棵 trie。核心优化是 range-driven memo_node：计算根时不 collect 全量 entries，而是用 BTreeMap::range() 按前缀定位子树，脏子树重算、干净子树直接命中缓存。Snapshot merge 是同步 in-place apply，在 period 边界耗时约几十 ms。

关键源文件：

- `state.rs` — 三段状态管理、merge 逻辑、checkpoint 序列化
- `incremental.rs` — IncrementalTrie 及 range-driven 根计算
- `trie.rs` — Conflux 特有的 merkle 哈希规则（非标准 MPT，无 RLP）
- `key_codec.rs` — canonical / delta-mpt 双空间 key 编解码

Conflux merkle 与以太坊 MPT 的显著差异记录在 `docs/TRIE-SPEC.md`。

## 开发与测试

### 测试体系总览

minimal-mpt 的正确性保障分四层，从快到慢：

1. **单元测试**（秒级）— 覆盖增量根对拍、三段轮转、key roundtrip、前缀查询等核心语义
2. **Fuzz**（分钟级）— 随机操作序列下的不变量检查
3. **verify-incremental 重放**（小时级）— 真实链数据上逐 commit 增量根 vs 全量根对拍
4. **全链历史重放**（天级）— 最终端到端验证，每个 epoch 的状态根与链上记录对比

### 单元测试

```bash
cargo test --release -p cfx-minimal-mpt
```

测试分布在三个位置，按验证对象组织：

`tests/api.rs`（12 个测试）覆盖公开 API 层面的正确性：
- 三段轮转完整流程（`commit_rolls_delta_to_intermediate_then_snapshot`）：用 snapshot_epoch_count=2 构造快速轮转，验证 delta→intermediate→snapshot 的根演变和数据可见性
- 分层读取优先级（`layered_precedence`）：同一 key 在 snapshot/intermediate/delta 各有值时，delta 优先
- 写入顺序无关性（`set_order_does_not_change_committed_root`）：正序和逆序写入 32 个 key 后 commit 根相等
- Key 编解码（`key_codec_roundtrips_all_shapes`）：Account/StorageRoot/Storage/CodeRoot/Code × Native/Ethereum 的 canonical↔delta-mpt 双向可逆
- Checkpoint 持久化（`file_store_recovers_latest_only`）：写入后重建 StateManager，读回数据一致
- 前缀查询和前缀删除的语义差异：snapshot 中的 key 可被前缀定位，delta 中的不能（因为 delta key 有 padding 前缀）

`src/incremental.rs`（5 个测试）覆盖增量根算法：
- `matches_oracle_under_random_ops`：4000 次随机插入/删除，每几步与全量 `trie_root` 对拍
- `matches_oracle_every_op`：同上但每步都对拍，确保单次更新不能隐藏在后续全量重算中
- `from_delta_matches_oracle`：从已有 BTreeMap 构造 IncrementalTrie，根与全量一致
- `empty_after_clear_is_null`：清空后根回到 MERKLE_NULL_NODE
- `bench_incremental_vs_oracle`（`--ignored`）：2000/6000 key 规模下增量 vs 全量的耗时对比

`src/trie.rs`（2 个测试）覆盖 merkle 哈希基本性质：空 trie 根为 null，插入顺序无关。

### Fuzz

```bash
cd crates/minimal-mpt/fuzz
cargo +nightly fuzz run state_ops            # 三段状态随机操作 + root 正确性
cargo +nightly fuzz run incremental_trie     # 增量根 vs 全量重算对拍
cargo +nightly fuzz run layered_state_ops    # 跨层读写 + 轮转
cargo +nightly fuzz run large_trie_ops       # 大 trie 边界条件

cd crates/replay-data-extractor/fuzz
cargo +nightly fuzz run encode_decode        # 编解码可逆
cargo +nightly fuzz run raw_packet           # 原始 packet 解析鲁棒性
```

默认无限运行，用 `-- -max_total_time=120` 限时。`state_ops` 覆盖面最广（三段轮转 + commit + root 一致性），日常修改后跑 2 分钟就够捕捉大多数回归。

### verify-incremental 模式

构建时加 feature，在真实链数据上做逐 commit 对拍：

```bash
cd oracle/conflux-rust/crates/replay-data-executor
cargo build --release --no-default-features --features backend-minimal-mpt,verify-incremental

cfx-replay-exec \
    --input ./data/packed-full \
    --config oracle/conflux-rust/run/hydra.toml \
    --checkpoint-every-groups 10
```

每次 `IncrementalTrie::root()` 调用后，用同一份 entries 从零全量计算一次 `trie_root` 并 assert 相等。覆盖 delta 和 snapshot 两条增量路径。速度约降 2×，跑几万 epoch（约 10-20 个 period 边界）即可覆盖 merge 后增量根的正确性。

### 全链历史重放

最终端到端验证。用正常构建（不加 verify-incremental）跑完一段或全部 `.cfxpack` 数据：

```bash
cd oracle/conflux-rust/crates/replay-data-executor
cargo build --release --no-default-features --features backend-minimal-mpt

cfx-replay-exec \
    --input ./data/packed-full \
    --config oracle/conflux-rust/run/hydra.toml \
    --checkpoint ./ckpt.bin \
    --checkpoint-every-groups 100 \
    --checkpoint-every-seconds 600
```

每个 epoch 产出的组合状态根与链上 deferred state root 做 4 字节前缀对比。运行结束时输出验证摘要：

```
replay progress t=2562s height=23694000 ... state_match=4580000/4580000 longest_mismatch_run=0
```

`state_match=N/N` 且 `longest_mismatch_run=0` 即全部通过。如果 `longest_mismatch_run` 持续大于 0，说明存在实现分歧。孤立的少量 mismatch 通常是链上被 blame 的区块——其 deferred root 被网络拒绝，正确的重放与之不同是预期行为。

### 改了什么，跑什么

| 修改区域 | 最低验证 | 推荐验证 |
|----------|---------|----------|
| `trie.rs` | 单元测试 | + verify-incremental 短段重放 |
| `incremental.rs` | 单元测试 | + fuzz `incremental_trie` 2min + verify-incremental |
| `key_codec.rs` | 单元测试 | + fuzz `state_ops` 2min |
| `state.rs`（merge/轮转） | 单元测试 + fuzz `layered_state_ops` | + 全链重放跨几个 period 边界 |
| 执行器逻辑 | 全链重放一段区间 | 全链重放 |
