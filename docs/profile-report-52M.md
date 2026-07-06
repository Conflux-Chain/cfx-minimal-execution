# Replay 性能 Profile 报告

**采样高度** epoch 52,618,000 – 52,658,000（2022-08-26 前后）
**Snapshot 规模** 62.2M 条 → 62.3M 条（期间净增 ~167k 条）
**采样方法** pprof-rs 997 Hz CPU 采样 + `MMPT_MERGE_TIMING` / `MMPT_DELTA_TIMING` 墙钟计时
**运行环境** 16 vCPU, 61 GB RAM, 无 swap

> 该区间已远离 epoch 40M 附近的合约循环炸弹密集区。密集区内单线程 EVM 被 gas-loop 交易卡住，吞吐低至 ~15 eps；本次采样区间交易密度回归常态，吞吐稳定在 ~244 eps，反映的是**稳态**下的性能构成。

---

## 一、CPU 时间构成

总采样 125,114 次，覆盖 20 个 group（40,000 epoch），墙钟 164 秒。

### 1.1 按子系统分解

| 子系统 | 采样数 | 占比 | 说明 |
|--------|--------|------|------|
| EVM 执行 (cfx\_vm) | 75,934 | 60.7% | 解释器主循环、操作码分派、gas 计算 |
| MPT 状态树 (minimal-mpt) | 29,509 | 23.6% | 三段根计算、快照读写、merge |
| Checkpoint 序列化 | 13,699 | 10.9% | bincode 序列化整个状态（本次 20 组写一次；生产 100 组一次，折合 ~2.2%） |
| genesis 地址 hex 解析 | 10,268 | 8.2% | 每 epoch 重新解析常量地址，应缓存 |
| KZG trusted setup 加载 | 4,555 | 3.6% | 首次触及合约时加载 |

> 百分比之和超过 100% 是因为 cfx\_executor 包含 EVM + 状态操作，存在层级包含关系；上表按各子系统的独立采样去重后的数字列出。

### 1.2 MPT 内部拆分

MPT 的 23.6% 可以进一步拆为：

| 操作 | 占比 | 备注 |
|------|------|------|
| `root_with_policy`（snapshot merge hash + delta root） | 11.9% | 分两个调用点，见下 |
| `memo_node`（增量哈希节点遍历） | 10.4% | 是 root\_with\_policy 的主要工作 |
| `snapshot_get`（执行期间读快照） | 0.4% | BTreeMap 点查 |
| `insert` / `remove`（delta 写入） | 0.3% | |
| `apply_updates`（merge 插入阶段） | 0.1% | |

`root_with_policy` 被两处调用：
- **Snapshot merge hash**（每组边界一次）：按墙钟比例占 ~6.4%
- **Delta root**（每 epoch commit 一次）：按墙钟比例占 ~5.5%

---

## 二、Snapshot Merge 开销

每 2,000 epoch（一个 group）边界触发一次 merge：先将 intermediate 条目插入 snapshot BTreeMap（apply），再增量重算 snapshot merkle root（hash）。

### 2.1 时间占比

| 指标 | 数值 |
|------|------|
| **merge 占墙钟时间** | **6.9%**（11.3 s / 164 s） |
| 平均单次 merge | 567 ms |
| 其中 apply（BTreeMap 插入） | 23 ms（4%） |
| 其中 hash（增量重哈希） | 544 ms（96%） |

### 2.2 Merge 时间波动

Merge 耗时受当前 group 内状态变更数（intermediate 条目数）强烈影响：

| 区间特征 | 每组新增条目 | 单次 merge |
|----------|-------------|------------|
| 稀疏组（h=52,642k–52,658k） | ~200–800 | 164–332 ms |
| 常规组（h=52,620k–52,632k） | 1,000–26,000 | 332–745 ms |
| 密集组（h=52,634k–52,638k） | 33,000–40,000 | 1,098–1,745 ms |

密集组的 merge 峰值接近 1.7 秒（对应 ~8.2 秒的组总耗时），但即便在峰值情况下，merge 也未超过组时间的 21%。增量哈希的设计使得 merge 代价与**被修改的子树规模**成正比，而非与快照全量成正比，因此 62M 条目的 snapshot 仍可保持亚秒级的常规 merge。

### 2.3 Delta Root 开销

Delta root 是每次 commit 调用的增量哈希，消耗与 delta 当前大小相关：

| 指标 | 数值 |
|------|------|
| 平均每次 commit | 229 μs |
| 20 组总计 | 9,162 ms（占墙钟 5.6%） |
| Delta 最大规模（组末尾） | ~11,600 条目 |

---

## 三、内存构成

在 epoch 53,320,000（checkpoint 高度）的实测数据：

| 项目 | 规模 |
|------|------|
| Snapshot 条目 | 64.4M |
| Intermediate 条目 | 2,887 |
| 序列化 KV 原始数据 | 4.0 GB（keys 2.8 GB + vals 1.2 GB） |
| Checkpoint 文件 | 4.7 GB |
| **进程 RSS** | **25.0 GB** |
| Checkpoint 写入峰值 | 29.2 GB（+4.4 GB 临时） |

### 3.1 RSS 分解估算

| 组件 | 估算大小 | 占比 | 来源 |
|------|----------|------|------|
| **Snapshot BTreeMap** | **~14.5 GB** | **58%** | 见下方明细 |
| **Hash cache (FxHashMap)** | **~8.5 GB** | **34%** | 非 singleton 子树哈希缓存 |
| 执行引擎 + cfxpack 缓冲 | ~2 GB | 8% | EVM 栈、交易缓冲等 |

### 3.2 BTreeMap 内存明细

4.0 GB 的原始 KV 数据，在 BTreeMap 中膨胀到 ~14.5 GB（~3.6 倍）。膨胀来自三处：

**a) 堆上数据膨胀：nibble 展开（+2.6 GB）**

IncrementalTrie 把每个 byte key 转为 nibble 表示（每字节拆成两个 nibble 字节），key 堆数据从 2.8 GB 翻倍到 5.4 GB。值的堆数据 1.2 GB 不变。堆上数据合计 6.6 GB。

**b) BTreeMap 节点 inline 开销（~4.8 GB）**

BTreeMap\<Vec\<u8\>, MptValue\> 每个条目 inline 存储一个 Vec\<u8\>（24 字节：指针 + 长度 + 容量）和一个 MptValue（24 字节：enum 判别 + Box\<\[u8\]\> 胖指针），共 48 字节。这些指针连同 B-tree 节点元数据（父指针、空槽、对齐填充）一起分摊在叶节点中。Rust BTreeMap 的叶节点每个 ~544 字节容纳 ≤11 条目，64.4M 条目约需 ~800 万个叶节点 + ~73 万个内部节点，合计 ~4.8 GB。

**c) 堆分配器开销（~3.1 GB）**

64.4M 个 key 和 64.4M 个 value 各有一次独立堆分配（合计 1.29 亿次 `malloc`）。每次分配有：
- size-class 对齐浪费（如 19 字节的 value 被 malloc 圆整到 32 字节，浪费 13 字节）
- 分配器簿记元数据（glibc malloc 每次 ~16 字节头部）

按 key 和 value 的实际长度分布加权估算，分配器开销约 ~3.1 GB。

> 最大的单项杠杆是 **nibble 展开**（+2.6 GB），其次是 **1.29 亿次独立堆分配的分配器开销**（~3.1 GB）。这两项合计占 BTreeMap 总内存的 39%。

### 3.3 Hash Cache

`IncrementalTrie` 的 `cache: FxHashMap<Vec<u8>, H256>` 缓存每个非 singleton 子树节点的 merkle 哈希，用于增量根的剪枝——命中缓存的干净子树无需递归。

Snapshot 使用 `CachePolicy::SkipSingleton` 策略跳过叶节点缓存以节省内存，但 64.4M 条目的 trie 仍有数千万个多子节点分支点需要缓存。每个缓存条目约 ~130 字节（nibble 前缀 Vec + 32 字节 H256 + FxHashMap 桶开销），RSS 减去 BTreeMap 和执行引擎后剩余约 8–9 GB，对应约 6,500 万个缓存节点。

### 3.4 Checkpoint 写入峰值

写 checkpoint 时 RSS 从 25 GB 飙升至 29.2 GB（+4.4 GB）。这是因为 bincode 序列化需要将整个 `PersistedState`（snapshot BTreeMap 的 key 从 nibble 转回 byte 形式）写入一个临时 Vec 缓冲后再落盘。这个峰值与 checkpoint 文件大小（4.7 GB）接近，属于预期行为。

---

## 四、观察与建议

### 已验证的结论

1. **Snapshot merge 开销可控。** 稳态下占墙钟 6.9%，在增量哈希的加持下，62M 条目的 snapshot 合并平均仅需 567 ms。瓶颈不在 merge 本身，而在 EVM 单线程执行（60.7%）。

2. **内存由 snapshot BTreeMap 和 hash cache 主导。** 两者合计占 RSS 的 92%。原始数据仅 4 GB，但 nibble 展开（+2.6 GB）、1.29 亿次独立堆分配的分配器开销（~3.1 GB）、BTreeMap 节点结构（~4.8 GB）和 hash cache（~8.5 GB）将其放大到 23 GB。

3. **Checkpoint 写入的内存冲击是硬性的。** +4.4 GB 峰值无法避免（需序列化整个 snapshot），在 61 GB 机器上当前 snapshot 规模已用 29 GB 峰值，后续增长需关注。

### 可优化的方向

| 方向 | 预期收益 | 复杂度 |
|------|----------|--------|
| 缓存 genesis 合约地址（消除每 epoch 的 hex 解析） | CPU -8% | 低 |
| Nibble key 改为 compact 编码或保持 byte 存储 | 内存 -2.8 GB | 中 |
| 将 snapshot BTreeMap 替换为 LMDB（已有 `snapshot-lmdb` feature） | 内存从 ~23 GB 降至缓存 + mmap，同时消除 checkpoint 峰值 | 中 |
| Streaming checkpoint 序列化（避免 Vec 缓冲） | 消除 checkpoint 写入 +4 GB 峰值 | 低 |

---

## 附录 A：稳态运行内存实测（150M 高度尺度，2026-07-06）

上文第三节的内存实测在 epoch ~53M（snapshot 64.4M 条）尺度。链已增长到接近链尾，本附录在 **150M 高度尺度**（snapshot 250M 条，约 3.9× 于 53M 时）重新实测运行期内存及其分布，方法与约束不同，故单列。

**采样方法**
- **运行期稳态**：从 `ckpt_140460000` 恢复后前向执行 208,000 epoch（104 个 group，~590 eps/s），每 1s 采 `/proc/<pid>/status`（VmRSS/VmHWM）与 `smaps_rollup`。
- **加载/恢复分布**：只读恢复生产 checkpoint（高度 150,052,000），`MMPT_RECOVER_TIMING=1` 打印各段条目数与恢复耗时。

### A.1 运行期内存（active execution，高度 140.46M → 140.67M）

| 指标 | 数值 |
|------|------|
| 加载峰值 VmHWM | **84.3 GB（80.4 GiB）** —— 发生在恢复期 `snapshot_root` 重建阶段，非执行期 |
| 稳态 VmRSS（执行期） | **73.81 GB（70.4 GiB）** |
| smaps Pss | 73.81 GB（≈ VmRSS，几乎无共享页） |
| Private_Dirty / Anonymous | 73,800 MB（**占 RSS 99.98%**，全部为堆） |
| Private_Clean（代码/rodata） | 12.2 MB |
| Shared_Clean（共享库） | 2.3 MB |

**关键观察：运行期 RSS 平坦，无 delta sawtooth。** 每 ~3.4s 跨越一个 2,000-epoch group 边界（delta 归 0 → 重建）时，1s 分辨率下 RSS 无可见波动（稳定在 73,814,992 kB）。delta trie 在一个 period 内增长到约 1 万条（数十 MB）后于边界旋转归并入 snapshot，这部分内存被分配器 arena 吸收复用，不体现为进程 RSS 的锯齿。**输入 `.cfxpack` 走缓冲 IO，进入 OS page cache（buff/cache ~40 GB），不计入进程 RSS。**

### A.2 加载/恢复条目数与内存（高度 150,052,000）

`MMPT_RECOVER_TIMING=1` 实测（生产 checkpoint 只读恢复）：

| 项目 | 数值 |
|------|------|
| Snapshot 条目 | **250,008,004** |
| Intermediate 条目 | 11,295 |
| Delta 条目 | 0（period 边界） |
| Hash cache 节点数（memo 内部节点） | **99,238,783** |
| Checkpoint 文件 / 读入原始字节 | 20.78 GB |
| after_read RSS | 20.29 GB |
| after_deserialize RSS | 45.9 GB |
| **恢复后常驻 RSS** | **110.9 GB（105.8 GiB）** |
| **恢复峰值 VmHWM** | **122.2 GB（116.5 GiB）** |

### A.3 RSS 分布归因（250M 尺度）

进程 RSS 几乎 100% 为匿名堆，由 snapshot IncrementalTrie 主导：

| 组件 | 估算 | 依据 |
|------|------|------|
| **Snapshot BTreeMap**（250.0M 条） | **~75–85 GB** | 沿用 A.2/三节实测的 ~3.6× KV 膨胀模型，按 250M 条线性放大 |
| **Snapshot hash cache**（99.24M 节点） | **~12–13 GB** | 实测缓存节点数 × ~130 B/节点 |
| Intermediate trie（11,295 条） | 数 MB | 可忽略 |
| Delta trie（0 → period 内 ~1 万条） | 数十 MB | 被分配器吸收，不体现 RSS 锯齿 |
| commitments / executed_epochs 有界窗口 | < 1 GB | 由 `--anomaly-streak` / `--max-mismatches` 界定的少量 epoch root |
| 执行引擎 + cfxpack 解码缓冲 | ~2 GB | EVM 栈、交易缓冲 |
| 代码 / rodata | ~14 MB | smaps Private_Clean + Shared_Clean |
| 输入 `.cfxpack` | 不计入 RSS | 缓冲 IO → OS page cache |

### A.4 恢复 `snapshot_root` 成本拆分（250M 条，实测）

恢复时对 250M 条 snapshot 重建 merkle root 是加载阶段的主要单项开销。`root_with_policy` 命中**并行**重建路径（`entries=250,008,004 ≥ PARALLEL_REBUILD_THRESHOLD=4096`，已实测确认走 `memo_node_rebuild` 而非串行 `memo_node`），拆分为：

| 阶段 | 耗时 | 占 snapshot_root | 并行度 |
|------|------|------------------|--------|
| `snapshot_hash`（`memo_node_rebuild` 并行 keccak） | **196,364 ms** | **91.7%** | 起步饱和全部 32 核（mpstat idle→0.00%） |
| `snapshot_cache_extend`（99.24M 条串行 extend 进 FxHashMap） | **13,760 ms** | 6.4% | 单核 |
| `snapshot_root` 合计 | 214,039 ms | 100% | |

其余恢复段：`snapshot_build`（条目 Vec 构造）59,250 ms、`intermediate_root` 14 ms、`delta_inc_build` 0 ms；`from_persisted_total` 273,303 ms。

> **结论：恢复期 `snapshot_root` 由并行 keccak 哈希主导（196s，92%），而非串行 cache 构建（13.8s，6%）。** 即便哈希已并行化（起步能打满 32 核），250M 条 + 99M 内部节点的 keccak 工作量本身仍是恢复墙钟的最大单项；串行的 `cache.extend` 只是次要项。降低恢复时间应针对哈希阶段（更深的并行切分 / 减少重哈希），而非 cache 构建。
