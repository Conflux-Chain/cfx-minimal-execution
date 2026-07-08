# minimal-mpt 历史 commit 性能横向对比实验报告(2026-07-07)

## 背景与问题

timing 插桩重构(overlay 机制废除、`MMPT_TIMING`/`MMPT_PROFILE` 双原语进树)完成后,首次 op-profile
跑出的数字看起来比重构前的 `latest` 列差(read +7.8%、delta_root 71→75 ns/entry),引出三个问题:

1. **delta_root 为什么没有回落**——c14 已把 delta trie 的存储换回 BTreeMap(`FlatStore`),
   但 delta_root 没有回到 baseline 时代的 52 ns/entry;
2. **to_mmpt_key 重写是否引入了开销**(oracle 侧,该函数旧版有一次序列化/反序列化翻译);
3. **当前树相对重构前(a954fae)是否真的性能回退**——最初几跑期间有另一进程(提取器)抢占 CPU,
   单跑之间同代码波动高达 ±15%,无法下结论。

由于历次优化(smallvec/nibble/twolevel/bulk)都是内联改动、没有清晰的函数边界,采样归因
(火焰图)对"哪个 commit 变慢"分辨力有限。因此采用**历史 commit 横向对比**:所有 minimal-mpt
commit 与 oracle 接口正交,全部装进**同一个最新 oracle executor** 里,用同一负载测 wall-clock
口径的 per-op 表。

## 实验环境

| 项 | 值 |
|---|---|
| 机器 | Intel Xeon Platinum 8488C,32 vCPU,123 GiB RAM(横向对比期间无其他负载;首轮跑受提取器干扰,已废弃并标注)|
| 工具链 | oracle pin rustc 1.94(merge worktree `oracle/merge-only-conflux-v3.0.3`)|
| oracle | `minimal-execution-oracle` @ `3cf918187`(resolve_config_dep 合并已回退,备份于分支 `backup-resolve-config-dep-merge`)merge tag `v3.0.3`,原版 `build_replay.sh` 冲突策略(全 `--ours`)|
| executor | `cfx-replay-exec`,`--no-default-features --features backend-minimal-mpt`,同一构建环境,仅替换三个 path 依赖(minimal-mpt / replay-checkpoint / cfxpack)指向各列的代码快照 |
| 负载 | checkpoint `ckpt_85520000.bin`(8.6 GB,105,035,234 条 snapshot)恢复后重放 **85520000 → 85522000**(恰好 1 个 snapshot group,2000 epochs,5541 blocks,160,443 txs),`--checkpoint-every-groups 1 --stop-after-checkpoint` 在边界写 ckpt 后退出 |
| 输入 | `data/packed-full`(按文件名 epoch 范围跳过恢复高度之前的容器)|
| 环境变量 | `MMPT_PROFILE=1 MMPT_TIMING=1 MMPT_MERGE_TIMING=1 MMPT_BULK_W=16000000` |
| 校验 | 全部跑 state_match 2000/2000、0 mismatch |

## 实验方法

### 横向对比(sweep)

对每个代表性 commit:

1. `git archive <commit> Cargo.toml Cargo.lock crates | tar -x` 导出**独立快照**
   (带各自的 workspace 根;工作树全程不动,用户未提交的 WIP 不受影响);
2. 旧 commit(重构前)把 overlay 版 `state/{mod,rotation,prefix}.rs`
   (原 `scripts/prof_state[deprec]`,含 per-op profile 累加器)覆盖进快照——这是旧代码获得
   `[profile]` 表的唯一途径;current 列直接用树内 `diag.rs` 的 registry(两者机制等价:
   每 op 两次 `Instant::now` + 原子累加,dump 格式一致);
3. 在 PROD merge worktree 中 sed executor 的三个 path 依赖指向快照,`cargo build`;
4. 同负载跑一遍(current 跑 **3 遍**估方差),取边界处的 `[profile] cumulative` 表。

列的选取:`baseline(2ab4bb3)` → `nibble(b34c680)` → `twolevel(2d14c9c)` →
`prerefactor(a954fae,重构前最后状态)` → `current(工作树快照 = 重构 c1–c15 + 树 WIP + 插桩)`。
smallvec 与 baseline 几乎无差、bulk 只改加载路径,略去。

### 火焰图归因(辅助)

executor 以 `profile` feature(pprof-rs,997 Hz,SIGPROF)构建,恢复完成后开始采样,同负载跑一遍,
对 `profile.folded`(90,180 样本,2691 独立栈)做区域归因与 leaf 帧互斥归因。
产物:`data/replay-checkpoint/bench/results/opprof/{flamegraph.svg,profile.folded,prof_timing-refactor-flame.log}`。

## 完整结果

### 横向对比主表(安静机器,2026-07-07 23:21–23:47)

per_call 单位 ns;merge 的 apply/hash 为单次墙钟毫秒;exec 为恢复完成到边界 ckpt 的墙钟秒。
所有列 calls 完全一致(read 920,235 / snap_get 574,863 / write 672,993 / delta_root 2,000×384,365,748 entries / merge 1×367,861 entries)。

| 列 | read | snap_get | write | merge ns/ent(apply/hash)| delta_root ns/ent | exec |
|---|---|---|---|---|---|---|
| baseline (2ab4bb3) | 3680 | 1263 | 2559 | 32200(1129/10715ms)| **52** | 53s |
| nibble (b34c680) | 3549 | 1104 | 2419 | 36716(1063/12442ms)| 59 | 56s |
| twolevel (2d14c9c) | **3440** | **899** | 2677 | 44777(944/15527ms)| 71 | 64s |
| prerefactor (a954fae) | 3874 | 893 | 2888 | 42558(1278/14377ms)| 71 | 65s |
| current rep1 | 4279 | 917 | 2315 | 45821(1477/15378ms)| 79 | 73s |
| current rep2 | 4253 | 911 | 2290 | 45648(1486/15306ms)| 78 | 72s |
| current rep3 | 4226 | 913 | 2270 | 45080(1443/15139ms)| 78 | 72s |

**方差**:current 三跑 read 4226–4279(±0.6%)、delta 78–79、merge 45.1k–45.8k、exec 72–73s。
安静机器上单跑分辨率约 ±1%,表内 ≥3% 的差异均为真实差异。

### 作废数据:受 CPU 干扰的首轮(仅留档,勿引用)

12:04–15:07 的五列 sweep 与 22:55 的首跑与提取器并发,同代码波动实测:bulk vs latest
(代码几乎相同)merge 相差 15%、exec 相差 4s。该轮数字与安静轮系统性偏高约 0–8%,
结论以本报告主表为准。日志留在 `data/replay-checkpoint/bench/results/opprof/prof_*.log`。

### 火焰图归因(current 树,含结尾一次 ckpt save)

区域占比(全进程 90.5s 采样):

| 区域 | 样本占比 | 说明 |
|---|---|---|
| ckpt save 的 `NibbleCursor::to_canonical_bytes` | **44.9%**(≈40s)| **100% 在 save 路径**:每次存 ckpt 把 105M 条目从 nibble 表示逐条物化成 `Vec<u8>`(FlatMap/Tuples 迭代器管道 + 每条一次分配)。不影响热循环,但是每次 ckpt 写盘的独立大头 |
| delta_root(per-commit RootEngine)| 21.3% | 见下方拆解 |
| snapshot merge(边界)| 15.3% | 见下方拆解 |

delta_root 区域内部(leaf 帧互斥归因):

| 成分 | 占区域 |
|---|---|
| FxHashMap cache `get` + dirty 逐前缀驱逐 `remove_entry` + hash | **28.4%** |
| tiny_keccak keccakf | 20.3% |
| BTreeMap 前缀查询(`prefix_exists`/`prefix_bounds`/search/navigate,FlatStore)| ~19% |
| 迭代器管道(FlatMap/Flatten/try_fold/Tuples)| ~12% |
| nibble 操作(push/pack/common_prefix/logical_eq)| ~6% |

merge 区域内部:

| 成分 | 占区域 |
|---|---|
| FxHashMap cache `get`(105M 级 cache,逐节点 DRAM miss)+ `remove_entry` | ~32% |
| BTreeMap/two_level 查找 | ~31% |
| keccakf | 10.7% |
| smallvec extend(two_level inline tail)| 7.3% |

key 转换开销(全进程占比):`to_delta_mpt_key_bytes` 1.1%、`to_key_bytes` 0.1%、
**`to_mmpt_key`(oracle 侧,本次重写的)0.02%**、account-cache keccak 0.2%。

## 结论

1. **历史两级台阶(已知交换,非本次引入)**:
   - nibble:delta 52→59(+13%),merge 32.2k→36.7k——packed nibble 表示的路由成本;
   - twolevel:delta 59→71(+20%),merge →44.8k(hash 10.7s→15.5s)——换来 snap_get
     1263→899(−29%)与内存(见 profile-report-52M.md)。
2. **current vs prerefactor 存在真实回退(重构 c1–c15 + 树内 WIP 的合并效果)**:
   - delta_root **71→78 ns/entry(+10%,每 group +2.8s)**;
   - read **3874→4253(+10%)**;另注意 read 在 prerefactor 自身(a954fae)相对 twolevel
     已 +13%,两段叠加后比 twolevel 时代贵 24%;
   - merge +7%(apply 1.28→1.47s,hash 14.4→15.3s);
   - **write 2888→2292(−21%,全史最优)**——c14 delta 单份化(去掉 BTreeMap+delta_inc 双写)的真实收益;
   - 净效果:exec 65→72s(+11%)。
3. **to_mmpt_key 重写无回归**:全进程 0.02%,序列化翻译疑虑排除。StorageKey 换 ref 版的
   收益上限约为全套 key 转换的 1.4%(占 read+write 区域约 18%),值得做但非大头。
4. **delta_root 回不到 52 的原因**:keccak 只占区域 20%;成本主体是 walk 的缓存操作
   (28%,其中 dirty 驱逐 `remove_entry` 占 11.7%)与 FlatStore 上的 BTreeMap 前缀查询(19%)
   ——这些属于 nibble/twolevel 阶段改掉的"表示法与缓存方案",不随 c14 的存储回换而恢复。

## 优化候选(按预期收益排序)

1. **ckpt save 的 `to_canonical_bytes` 物化**(~40s/次写盘):save 路径直接流式输出
   canonical bytes,避免逐条 Vec 分配与迭代器深链;
2. **delta_root 的 dirty 驱逐方案**(区域 ~28% 的缓存操作):逐前缀物理 remove 改为
   子树合并驱逐或世代戳失效;
3. **FlatStore 的前缀路由**(区域 ~19%):walk 每节点两次 O(log n) range 查询,
   对比 two_level 的定制段扫描——这是 c14 之后 delta walk 的新增成本来源之一;
4. **merge.hash 的 cache 查询局部性**(区域 ~32%):105M 级 FxHashMap 逐节点 get 的 DRAM miss;
5. read 路径 +10%+13% 两段回退的定位(见下一步)。

## 下一步

- 跑 `1884c2e`(纯重构终点,c15)一列,把 current 的回退拆成"重构"与"树内 WIP"两份账;
- 若重构侧占大头,沿 c10–c15(重点 c14)wall-clock 二分;
- read 的 prerefactor 段回退(twolevel→a954fae,+13%)单独定位(a954fae 是单个 tmp commit)。

## 附录:产物与复现

- sweep 脚本与全部日志:`/tmp/claude-1000/-cfx-minimal-execution/599df3e8-.../scratchpad/sweep/`
  (`summary.txt` 为汇总;`bin_<label>` 为各列二进制;`results/prof_<label>_<rep>.log` 为原始日志)
- 火焰图:`data/replay-checkpoint/bench/results/opprof/flamegraph.svg`(+ `profile.folded`)
- 单列复现:`scripts/run_op_profile.sh <ckpt.bin> <cfxpack-dir> [label]`
  (构建走 `scripts/build_replay.sh`,插桩即树内 `MMPT_PROFILE=1`)
- 历史列复现要点:导出快照 + overlay `state/{mod,rotation,prefix}.rs`(备份见 sweep 目录同级
  `deprec-backup/`)+ sed executor path 依赖。
