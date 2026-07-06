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

### A.2 加载/恢复条目数与内存（高度 150,052,000，生产 streaming 路径）

> **订正（stream / non-stream 混淆）**：本节此前记录的「恢复后常驻 110.9 GB / 峰值 122.2 GB」以及 `after_read 20.29 GB`、`after_deserialize 45.9 GB` 两步，来自 **non-streaming 的诊断工具 `ckpt-restore-rss`**（`fs::read` 整个 checkpoint 进内存 + `bincode` 反序列化出中间 Vec + 再建 trie，三者同时驻留）。**生产恢复走 `Checkpoint::load_streaming`（`State::from_reader`，`BufReader` 直接跨文件，不 `fs::read` 整文件）**，二者内存差 ~37 GB，故此前的数字不代表生产内存。下表为 streaming 路径实测（`ckpt-mem-survey` / `ckpt-bench`，同一 20.78 GB checkpoint）。

| 项目 | 数值（streaming 生产路径） |
|------|------|
| Snapshot 条目 | **250,008,004** |
| Intermediate 条目 | 11,295 |
| Delta 条目 | 0（period 边界） |
| Hash cache 节点数（memo 内部节点） | **99,238,783** |
| Checkpoint 文件大小 | 20.78 GB（19.35 GiB） |
| **加载后常驻 RSS** | **73.8 GiB** |
| **加载峰值 VmHWM** | **84 GiB**（`snapshot_root` 并行重建时的 `cache_entries` 瞬时 Vec） |

> 对照：non-streaming `ckpt-restore-rss` 工具在同一 checkpoint 上常驻 110.9 GB / 峰值 122.2 GB —— 仅用于离线诊断，**不是**生产路径。

### A.3 RSS 分布归因（250M 尺度，`ckpt-mem-survey` 单趟实测 + glibc chunk 模型）

进程 RSS 几乎 100% 为匿名堆，由 snapshot `IncrementalTrie` 主导。下表所有「实测」行来自对 `entries`/`cache` 的单趟遍历（`memory_survey`）；「计算」行由实测长度直方图套 glibc chunk 公式 `max(32,(n+8+15)&~15)` 得出；「模型」行为对 73.8 GiB 总量的残差归因。**总量与实测 RSS 对齐（Σ ≈ 73.8 GiB）。**

| 组件 | GiB | 来源 |
|------|-----|------|
| Entry key nibble 数据 | **24.80** | 实测 Σ key.len()（nibble，2×字节） |
| Value 数据 | **3.22** | 实测 Σ value.len() |
| Cache key nibble 数据 | **5.15** | 实测 Σ cache-key.len() |
| Cache H256（inline 于 hashmap 桶） | **2.96** | 实测 32 × 99.24M |
| —— *真实数据小计* | **36.13**（49%） | —— |
| Key buffer 分配器开销 | 3.24 | 计算（chunk−data，键多为 112–114 nibble→128B chunk） |
| Value buffer 分配器开销 | **5.36** | 计算（208M 个 ≤24B 小值各占 32B 最小 chunk） |
| Cache-key buffer 分配器开销 | 0.77 | 计算 |
| BTreeMap inline 头（`Vec<u8>`24 + `MptValue`16 = 40B×250M，已占用槽） | 9.31 | 实测类型尺寸 |
| BTreeMap 节点结构 + ~54% 填充浪费 + 节点分配 | ~13.3 | 模型（`from_snapshot_reader` 顺序 `.insert()` → 升序插入叶约 54% 满） |
| Cache hashmap 表（`Vec`头 2.22 + 空桶 1.82 + control 0.13） | 4.17 | 计算（99.24M 条 → 2²⁷=134.2M 桶，7/8 负载） |
| Intermediate / delta / 执行引擎 / cfxpack 缓冲 | ~1.5 | 估算 |
| **合计** | **~73.8** | ≈ 实测 RSS |

**结论：73.8 GiB 中仅约 36 GiB（49%）是真实 key/value/cache 数据，另外 ~38 GiB（51%）是开销**，其中最大的三块指针/对齐/结构开销是：① inline 指针头（`Vec`+`MptValue` 共 11.5 GiB）；② glibc chunk 对齐（9.4 GiB，小值的 32B 最小 chunk 是主因）；③ BTreeMap 升序插入的 ~54% 填充浪费（~7.9 GiB，可用 `FromIterator` 批量构建修掉）。

### A.4 恢复 `snapshot_root` 成本拆分（250M 条，streaming 路径实测，并行 fix 之后）

`root_with_policy` 命中**并行**重建路径（`entries=250,008,004 ≥ PARALLEL_REBUILD_THRESHOLD=4096`，实测确认走 `memo_node_rebuild`）。**按子树规模 fan-out 的并行修复落地后**，`snapshot_hash` 由旧的 196s 降到 54s（3.6×）：

| 阶段 | 耗时 | 说明 |
|------|------|------|
| `stream_read_snapshot`（流式读 250M 条建 BTreeMap） | **127.3 s** | 单核解析 + `.insert()` |
| `snapshot_hash`（`memo_node_rebuild` 并行 keccak） | **54.0 s** | 起步打满全部核（旧值 196s，见并行 fix） |
| `snapshot_cache_extend`（99.24M 条 extend 进 FxHashMap） | **13.5 s** | 单核 |
| **加载合计** | **≈ 201.7 s** | —— |

> **结论：streaming 恢复墙钟由 `stream_read`（127s，63%）主导，其次才是并行 keccak（54s，27%）。** 并行 fix 已把哈希从加载瓶颈降为次要项；进一步降恢复时间应针对流式读取/建 BTreeMap 阶段（如批量构建、byte-key 免 nibble 展开）。

### A.5 内存优化潜力（`ckpt-mem-survey` 实测，先不计实现开销）

针对两个候选优化的实测潜力（裸潜力，未扣除新方案自身的指针/结构开销）：

**(b) key 前 40-nibble（20 字节）路由前缀去重**
- distinct 40-nibble 前缀 = **23,796,252**，vs 250M 条目 → 平均每前缀 ~10.5 条共享。
- 当前前缀占 `min(40,len)` × 250M = **9.31 GiB**；去重后每个 distinct 前缀只存一次 = **0.89 GiB**。
- **裸去重潜力 = 8.43 GiB** key 数据。（注：另有正交的更大杠杆——key 以 nibble 存储使体积翻倍，byte-pack 可把 24.80 GiB 减半。）

**(c) value 「u32 padding 到 32 字节」猜想 —— 实测不成立**
- value 长度分布是**变长**的（len 1 / 4 / 21 为主；恰好 32 字节的仅 158,866 条）。
- 全部 250M value 的 **Σ 前导零字节仅 73,884 B（0.00 GiB）** —— 值已是紧凑（RLP）编码，前导零早被剥离，**没有 padding 到 32 字节**。
- 46.1%（115M）的值数值上 <2³²，但它们当前平均只占 ~2.2 B；若强行按定长 u32(4B) 存反而**更大**（0.24 GiB → 0.43 GiB）。
- **结论：value 侧无 padding 浪费，优化潜力 ≈ 0。**
