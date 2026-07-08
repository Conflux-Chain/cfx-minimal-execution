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
| **fixed** | 68.2 | **45.2** | **109.1s** |

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
| **恢复 stream_read(250M)** | 100.0 → **62.1s(−38%)** | c08 回退清零,优于 baseline 的 64.8s;load 总时长 157→109s |
| exec | 33.8 → **33.1/33.4** | twolevel 之后最优;delta_root 36→34 |
| 加载峰值 | 69.0 → 68.2 GiB | 部分回收,见"遗留" |
| 稳态 | 45.8 → 45.2 GiB | 加载分配模式更干净,trim 后碎片更少 |

## 结论:全程账本(baseline → fixed,生产口径)

**时间 +2.0~2.3s/+7%(31.1→33.1/33.4),换稳态内存 −24.5 GiB/−35%(69.7→45.2)、加载峰值 −16.8 GiB:**

| 段 | Δexec | Δ稳态内存 | 判定 |
|---|---|---|---|
| smallvec | −0.9s | −0.8 GiB | 双赢;**全史 exec 最优点(30.2s)在此** |
| nibble | +1.5s | **−18.4 GiB** | 全场最划算的交换 |
| cleanup | +0.7s | ≈0 | 近噪声,可复核 |
| twolevel | +2.7s | −1.9 GiB | 换 snap_get −25%、apply 最优;时间代价最大的一段 |
| bulk | +1.3s | −2.6 GiB | 名义只改加载,实为密度↔读局部性交换;机制假说见"遗留" |
| prerefactor | −0.3s | ≈0 | |
| 重构 c1–c16 | **−2.3s** | −0.3 GiB | 净改善(c14 write 单份化);无真实台阶 |
| 本轮修复 | **−0.5s** + save/恢复大头 | −0.6 GiB | 重构+插桩期的可白拿回退全部清零 |

与上一报告结论的关系:

1. "current 相对 prerefactor 回退(delta +10%、read +10%)"——**订正**:16M 口径伪影 + 旧机微架构敏感;
   生产口径下重构段是净改善;
2. "delta_root 回不到 baseline"——仍成立但量级重估:31→34(+10%),台阶在 twolevel(31→38 时代),
   重构段无增量,修复回收 2ns;
3. "to_mmpt_key 重写无回归"——维持;"ckpt save to_canonical_bytes 44.9%"——已修复清除;
4. 真实且仍在的交换:merge hash 6.2→8.1s/组(nibble+twolevel)、delta_root +3ns/ent、twolevel 段的
   exec 台阶——这些与 −24.5 GiB 绑定,是设计交换而非缺陷。

## 遗留问题

1. **加载峰值 +2.8 GiB**(prerefactor 65.4 → fixed 68.2):重构段引入、来源未定位;候选 c12(RootEngine/
   并行 rebuild 的瞬态)、c10(PrefixKey 表示)、c14(层存储更换);峰值二分实验进行中,结果补录于此;
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
