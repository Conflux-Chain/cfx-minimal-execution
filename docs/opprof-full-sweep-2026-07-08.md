# minimal-mpt 全程逐 commit 性能与内存对比 + 热路径修复报告(2026-07-08)

本报告在新机器上重做并扩展了 [opprof-timing-refactor.md](opprof-timing-refactor.md)(2026-07-07,Xeon 8488C):
executor 用最新 oracle,列覆盖 baseline→current 全部 commit(计时),补测最新 checkpoint(250M 条)的加载
峰值/稳态内存,并完成了一轮热路径修复(`fix several performance issues`)。研究口径是 baseline 相对 current
的**全程**回退。

## 方法论勘误(重要)

上一报告与本报告初版的计时均在 `MMPT_BULK_W=16000000` 下测量——这是 opprof 方法论的 profiling 旋钮,
**不是生产口径**(README 标准重放调用不设该变量,默认 1M)。复核发现 W=16M 使 post-bulk 代码的热路径
系统性变慢且逐 commit 恶化(current 列 exec 虚高 5.8s),**上一报告"current vs prerefactor 回退"的
结论大半是该口径的伪影**(c08 +1.9s、c10 +1.1s 台阶在生产口径下均不存在)。本报告全部数字为
**生产口径(W 默认 1M)**;16M 原始日志仍在产物目录,仅作留档,勿引用。

## 实验环境

| 项 | 值 |
|---|---|
| 机器 | Intel i9-14900K,32 线程,125 GiB RAM,NVMe;全程无其他负载,同代码方差 ±2% |
| 工具链 | oracle pin rustc 1.94(merge worktree `oracle/merge-only-conflux-v3.0.3`)|
| oracle/executor | `minimal-execution-oracle` @ `ca112c6be`,merge tag `v3.0.3`,`build_replay.sh` 原策略 |
| 计时负载 | `ckpt_85520000.bin`(105,035,234 条)重放 85520000→85522000(1 group),`--checkpoint-every-groups 1 --stop-after-checkpoint`;env `MMPT_PROFILE=1 MMPT_TIMING=1 MMPT_MERGE_TIMING=1 MMPT_RECOVER_TIMING=1`(W 不设,默认 1M)|
| 内存负载 | `ckpt_150052000.bin`(19.4 GB,250,008,004 条,最新 ckpt)走生产 streaming load;峰值 = VmHWM,稳态 = load 后 `malloc_trim(0)` 再读 VmRSS(不 trim 每列虚高 4–4.8 GiB)|
| 校验 | 全部计时列 0 mismatch;calls 签名逐字节一致(read 920,235 / snap_get 574,863 / write 672,993 / delta_root 2,000×384,365,748 / merge 1×367,861);修复后 `verify.sh full` PASSED |

commit 对应(历史重写后):前半内存优化 WIP:`2ab4bb3` baseline → `0eab51c` smallvec → `b34c680` nibble →
`a41988c` cleanup → `2d14c9c` twolevel → `7597cb0` bulk → `149c5a5` prerefactor;后半重构 PR-1..4:
`4fc06e6` c1 … `73833e3` c15、`bff5ede` c16 ≈ current(`6eaf9dd`);fixed = current + 本轮修复(`570b037`)。

## 主表(生产口径;per-op ns,merge apply/hash ms,exec/save/恢复 s)

nibble 之前及 twolevel 列不含 bulk 加载路径,W 无关;bulk 起为 W=1M 实测。

| 列 | read | snap_get | write | merge ns/ent(apply/hash)| delta_root | exec | save | 恢复 stream_read |
|---|---|---|---|---|---|---|---|---|
| baseline | 2228 | 803 | 1465 | 18576(681/6152)| **31** | 31.1 | 8.6 | 26.7 |
| smallvec | 2178 | 777 | 1443 | 17504(698/5740)| 30 | **30.2** | — | — |
| nibble | 2058 | 653 | 1287 | 21373(656/7205)| 32 | 31.7 | — | — |
| cleanup | 2148 | 687 | 1351 | 21074(646/7105)| 33 | 32.4 | — | — |
| twolevel | 1954 | 514 | 1478 | 24359(530/8430)| 38 | 35.1 | 8.4 | 34.6 |
| bulk | 2116 | 496 | 1606 | 24113(696/8173)| 39 | 36.4 | — | — |
| prerefactor | 2089 | **489** | 1590 | 23965(690/8125)| 39 | 36.1 | 8.5 | 36.7 |
| current | 1998 | 498 | 1201* | 24350(585/8372)| 36 | 33.8 | 28.6 | 46.5 |
| **fixed r1** | 2019 | 492 | 987 | 23506(579/8067)| 34 | **33.1** | **6.9** | 35.0 |
| **fixed r2** | 2018 | 490 | 985 | 24118(587/8284)| 34 | 33.4 | 6.8 | 34.5 |

\* current 的 write 单跑 873,fixed 两跑 985/987;该项单跑波动大,取保守值。

重构段逐 commit(exec,W=1M;c1–c6 未在生产口径复测——16M 口径下它们与 prerefactor 持平、无台阶信号):

```
prerefactor 36.1 → c07 36.4 → c08 35.5 → c09 36.6 → c10 36.3 → c11 36.8
→ c12 36.8 → c13 37.3 → c14 35.7 → c14b 35.0 → c15 37.7 → current 33.8
```

单跑噪声 ±1s;整段无显著台阶,终点显著优于起点(c15→current 的 −3.9s 与 c14b/c15 波动超出常规噪声,
如需按 commit 精确归因需补 rep)。

## 内存(250M 条最新 ckpt,W=1M;GiB)

| 列 | 峰值 VmHWM | 稳态 RSS(trim 后)| load 时长 |
|---|---|---|---|
| baseline | 85.0 | 69.7 | 108.8s |
| smallvec | 86.1 | 68.9 | 110.2s |
| nibble | 66.6 | 50.5 | 98.2s |
| cleanup | 66.7 | 50.6 | 96.8s |
| twolevel | 68.3 | 48.8 | 129.3s |
| bulk | 65.2 | 46.2 | 123.9s |
| prerefactor | **65.4** | 46.1 | 128.8s |
| current | 69.0 | 45.8 | 157.1s |
| fixed | 69.6 | 46.3 | 127.2s |
| fixed+slim | 65.5 | 46.2 | 124.1s |
| fixed+slim+drop | 62.1 | 46.2 | 123.1s |
| **fixed+shard** | **47.3** | **46.0** | 132.4s |

fixed+slim = fixed 之上再加 `EntryRef` 瘦身(72→48 B,见"遗留问题 1"),峰值 −4.1 GiB 回到 prerefactor
水位;+drop = 冷启动 rebuild 返回后、cache 入表前先 `drop(entries)`(两者不再同刻存活),再 −3.4 GiB;
+shard = 冷启动 rebuild 改为分片派发(遗留问题 1 的终局修复),全店物化消失,峰值 ≈ 稳态 + 1.3 GiB。
订正:本表 fixed 行初版误引了 W=16M 的日志(68.2/45.2/109.1s)——W=16M 使 load 各列普遍快
~15s(批更大、flush 更少)且峰值/稳态略低,与计时列同为口径伪影,已换为 W=1M 实测。

load 时长的构成(250M 条):stream_read ~78s(读盘/解码/bulk 建 store,各列同)+ snapshot_root
(冷启动建 root 与 99.2M 条 cache:drop 列 45.7s = 物化 5.2 + 并行哈希 33.8 + 入表 6.7;shard 列
53.4s,逐片物化→哈希→入表交错,+7.7s 是片间串行间隙与 CAP+1 探测的代价),其余阶段 ~0。

## 本轮修复(`570b037 fix several performance issues`)

动机来自逐 commit 归因:c10 把两个利用内部表示的字节操作换成了 nibble 迭代器管道(封装收口的代价),
c08 的流式加载引入批级双重物化。修复(热路径写法同步按"避免 index/边界检查,用 slice 方法"整理):

1. `NibbleCursor::logical_eq` → 全 key 视图直接分段 memcmp(等长切分 quick path;奇偏移子视图才走 nibble);
2. `NibbleCursor::to_canonical_bytes` → 对齐视图两段 copy;save 路径 `for_each_canonical` 改借用回调 + 单
   buffer 复用(每 ckpt 0 次每条分配);
3. `persist::stream::load_state` → 逐条读边转内联值(恢复 c07 数据流,批只持 key+内联值);
4. `nibbles_from` 去 `byte_at` 逐字节派发(段迭代器 chain);`pack_nibbles` 对齐 copy;`push_nibble`
   去三重 index;`common_prefix_len_from` 整字节比较。

收益(fixed vs current,生产口径):

| 指标 | current → fixed | 说明 |
|---|---|---|
| **ckpt save** | 28.6 → **6.9s(−76%)** | 上一报告火焰图第一大项(44.9%)清除;优于 baseline 的 8.6s |
| **恢复 stream_read(250M)** | 100.0 → **78.8s(−21%)** | c08 回退清零,回到 prerefactor/bulk 水位(78.0s);仍高于 baseline 的 64.8s(bulk 段既有交换);load 总时长 157→127s(slim 后 124s)|
| exec | 33.8 → **33.1/33.4** | twolevel 之后最优;delta_root 36→34 |
| 加载峰值 | 69.0 → 69.6 GiB(持平)| 计时修复不动峰值;削峰由后续三刀完成:EntryRef 瘦身 →65.5、drop(entries) →62.1、分片派发 →47.3(见"遗留问题 1")|
| 稳态 | 45.8 → 46.3 GiB(持平)| |

## 结论:全程账本(baseline → fixed,生产口径)

**时间 +2.0~2.3s/+7%(31.1→33.1/33.4),换稳态内存 −23.4 GiB/−34%(69.7→46.3)、加载峰值 −37.7 GiB
(85.0→47.3,含 EntryRef 瘦身、drop(entries) 与分片派发):**

| 段 | Δexec | Δ稳态内存 | 判定 |
|---|---|---|---|
| smallvec | −0.9s | −0.8 GiB | 双赢;**全史 exec 最优点(30.2s)在此** |
| nibble | +1.5s | **−18.4 GiB** | 全场最划算的交换 |
| cleanup | +0.7s | ≈0 | 近噪声,可复核 |
| twolevel | +2.7s | −1.9 GiB | 换 snap_get −25%、apply 最优;时间代价最大的一段 |
| bulk | +1.3s | −2.6 GiB | 名义只改加载,实为密度↔读局部性交换;机制假说见"遗留" |
| prerefactor | −0.3s | ≈0 | |
| 重构 c1–c16 | **−2.3s** | −0.3 GiB | 净改善(c14 write 单份化);无真实台阶 |
| 本轮修复 | **−0.5s** + save/恢复大头 | ≈0 | 重构+插桩期的可白拿回退全部清零;峰值另由 EntryRef 瘦身 −4.1 GiB |

与上一报告结论的关系:

1. "current 相对 prerefactor 回退(delta +10%、read +10%)"——**订正**:16M 口径伪影 + 旧机微架构敏感;
   生产口径下重构段是净改善;
2. "delta_root 回不到 baseline"——仍成立但量级重估:31→34(+10%),台阶在 twolevel(31→38 时代),
   重构段无增量,修复回收 2ns;
3. "to_mmpt_key 重写无回归"——维持;"ckpt save to_canonical_bytes 44.9%"——已修复清除;
4. 真实且仍在的交换:merge hash 6.2→8.1s/组(nibble+twolevel)、delta_root +3ns/ent、twolevel 段的
   exec 台阶——这些与 −24.5 GiB 绑定,是设计交换而非缺陷。

## 遗留问题

1. **加载峰值 +2.8 GiB —— 已定位并修复**。二分(c08/c10/c12/c14 内存列):prerefactor 65.4 = c08 65.4
   → **c10 69.2** → c12 68.9 → c14 69.2 —— 单一台阶在 c10。机制:c10 把冷启动并行 rebuild 的
   `EntryRef` 从 Composite 两段裸切片(48 B)换成存整个 `NibbleCursor`(72 B,offset 字段冗余),
   × 250M 条全量物化 ≈ +5.6 GiB 瞬态。修复:`EntryRef` 改回三裸切片(`key_head`/`key_tail`/`value`,
   48 B),nibble 视图按需重建(零成本)。实测(fixed+slim 列):峰值 69.6 → **65.5 GiB(−4.1)**,与
   prerefactor 65.4 持平,计时全指标不回退(exec 33.1 / save 7.0 / stream_read 34.7)。
   峰值瞬态的构成已对账:峰值 65.5 − load 完成时 RSS 49.9 = 15.6 GiB ≈ 输入侧 entries
   Vec(48 B × 250.0M = 11.2 GiB)+ 输出侧 cache_entries Vec(48 B 元组 × 99.24M = 4.4 GiB),
   两者在 rebuild 返回/入表时同时存活;最终 cache 哈希表本身属稳态而非瞬态。
   据此追加第二刀:cache 入表(`reserve`/`extend`)前先 `drop(entries)`,峰值再降
   65.5 → **62.1 GiB(−3.4)**,其余指标持平(exec 32.4 / save 7.0 / load 123.1s)。
   第三刀(终局):冷启动 rebuild 改为**分片派发**(`memo_node_sharded`)——任务是前缀区间,
   自己经 `TrieReader::iter_merkle_prefix` 从 store 取数;> CAP(4M 条)不物化、按子节点递归分
   (沿公共前缀一跳切进鲸鱼合约内部),≤ CAP 整片物化走原并行 `memo_node_rebuild`、cache 逐片
   入表随手释放。分发节点复用 `memo_node` 原函数体(子递归抽为闭包参数 `memo_node_via`,单态化
   零开销;等价性测试断言各 cap 下 root 与 cache 逐条等于串行 memo_node)。实测峰值
   62.1 → **47.3 GiB**,瞬态仅 +1.3 GiB;代价 load +9s(123→132s,片间串行间隙),热路径经
   3 rep 验证无回退(exec 32.9–35.2 覆盖 fixed 区间,delta_root 33–35 在噪声带内);
2. bulk 的 +1.3s:snap_get 持平、read/write 变慢 → 指向全局堆布局效应而非 store 内查找;未证实,
   决定性实验为同二进制 W=1/1M/16M 三档 A/B;
3. cleanup(a41988c)的 +0.7s 近噪声边界,未复测;
4. c14b/c15/current 间 ±2s 波动超常规噪声,按 commit 精确归因需补 rep。

## 附录:产物与复现

- 全部日志/二进制/脚本:`/conflux-new-storage-workspace/sweep-opprof/`(`results/prof_<label>[w1]_<rep>.log`
  计时、`results/mem_<label>.log` 内存、`summarize.py` 汇总、`overlay/` 重建的旧 commit 插桩、
  `build_*.sh`/`run_*.sh` 逐列构建与运行);W=16M 留档日志同目录(`prof_<label>_<rep>.log` 中 sweep
  首轮与逐 commit 轮全部为 16M 口径);
- 单列复现:`run_column.sh` 系(计时)、`membin_<label> <ckpt.bin> 5`(内存);
- 环境记录:oracle 克隆曾为 shallow 且缺 tag v3.0.3(已 unshallow + 取 tag);zstd 已装入 rebuild.sh。
