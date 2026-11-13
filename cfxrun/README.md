# cfxrun — 云上分段并行重放

把主网（或测试网）按 S3 checkpoint 切段，每段一台腾讯云按量实例并行跑 `cfx-replay-exec`。
执行器单进程跑完整段，本地约每 **1 小时**原地覆盖 `ckpt.bin`（crash 可续跑）；另有独立
计时器在墙钟累计超过 **24 小时**时，直接把现成 `ckpt.bin` 硬链快照、压缩归档，由主控
签发 multipart URL、worker 直传到 `s3://conflux-replay-txs/checkpoints/<run>/`。
成功段拉日志并确认上传后销机；失败段留机，用盘上最近一份 ckpt 排查。

## 完整路径（必须按这个走）

PoS 段裸编 oracle HEAD 会从第一个新执行 epoch 起 state 全错；未打标的 `.cfxpack` 会把无效头计成孤立 mismatch。

1. **在 Linux x86_64 上编两个二进制**（不要在 `oracle/conflux-rust` 里直接 `cargo build`）：

   ```bash
   TAG=v3.0.3 ./scripts/build_linux_bins.sh bin/
   # 等价于：
   #   TAG=v3.0.3 ./scripts/build_replay.sh bin/cfx-replay-exec --no-default-features --features backend-minimal-mpt
   #   cargo build --release -p cfxpack --bin label_blame --features label-blame
   ```

2. **plan / up / watch**，执行器用 merge 产物，并把 `label_blame` 带上：

   ```bash
   export CFXRUN_SSH_KEY=~/.ssh/your_key.pem
   export AWS_PROFILE=your-s3-profile       # 读输入桶 + 写输出桶
   export CFXRUN_OUTPUT_PROFILE=your-s3-profile  # 可选；不设则跟默认 credential chain
   python3 cfxrun/cfxrun.py plan <run>     # 默认 --window-hours 24 --local-ckpt-seconds 3600
   python3 cfxrun/cfxrun.py up <run> \
     --bin bin/cfx-replay-exec \
     --label-blame bin/label_blame \
     --config data/conflux-rust-config/hydra-v303.toml
   # --bin 必须是 build_replay.sh / build_linux_bins.sh 的 merge 产物（两个脚本都输出
   # 到 bin/cfx-replay-exec）；不要用 oracle/conflux-rust 里裸编的 oracle HEAD。
   python3 cfxrun/cfxrun.py watch <run>
   ```

3. **worker**：下载 `.cfxpack` → `label_blame` 写成 `.cfxpackb`（`FLAG_BLAMED`）→
   `--anomaly-streak 0` 重放。S3 上已是 `.cfxpackb` 则跳过打标。

4. **本地 1h / 上传 24h**：执行器**单进程跑到底**，`--checkpoint-every-seconds 3600`
   原地覆盖 `ckpt.bin`（不再每小时重启进程）；worker 另起计时器，墙钟累计达到
   `--window-hours`（默认 24）时把**现成的** `ckpt.bin` 硬链快照后压缩进 `archives/`，
   `watch`/`down` 用本地凭据签发 multipart URL，worker 直传
   `s3://conflux-replay-txs/checkpoints/<run>/<seg>/ckpt_<高度>.bin.zst`。
   归档高度取 ckpt 自身高度，与当前重放高度可差 ≤1h；不回内存重 dump。
   worker 只认**同时**有 `wrote checkpoint ... at height <h>` 的 `[ckpt] height=<h>`：
   执行器写盘失败时只打 warning 但仍打 `[ckpt]`，这种高度不会归档、也不会拿来续跑。
   未上传完**拒绝销机**（除非显式 `--discard-checkpoints`）。

### 切段数 vs 中间 ckpt（不要搞混）

| 位置 | 用途 | 本轮机器数 |
|---|---|---|
| `s3://cfx-minimal-execution/mainnet/cfxckpt/`（作者 22 个） | **`plan` 唯一切段来源** | **仍是 22 段 / 22 台** |
| `s3://conflux-replay-txs/checkpoints/<run>/`（本轮上传的中间 ckpt） | 以后做更密并行的素材 | **不自动增加本轮机器** |

上次 `full-1d` 留下的 T1/T8/T14 中间 ckpt 也在输出桶里，**不会**让这次 `plan` 变成 25 段。

### 系统盘

`plan` 已按打标膨胀估盘：默认下限 **100G**；pack 压缩体积很大的段（如 T0、T20）**120G**。
磁盘相对机器费可忽略，宁可大一点，避免打标后 `.cfxpackb` + 双份 ckpt 写爆盘。

### 实测开销（`soak-1h24h-20261112`）

一次写 ckpt 的停顿 ≈ `ckpt.bin` 体积 ÷ 磁盘写带宽，与机型共用一个盘：

| 段 | 机型 | `ckpt.bin` | 写一次 | 折算到 1h 间隔 | 归档 `.zst` |
|---|---|---|---|---|---|
| T2 | SA9.MEDIUM8 | 0.66 GB | ~20–25 s | ≈0.7% 吞吐 | 0.20 GB |
| T19 | MA9.2XLARGE64 | 18.2 GB | ~125 s | ≈3.4% 吞吐 | 5.35 GB |

归档压缩约 19 s（`zstd -T0`），相对 24h 窗口可忽略；快照走硬链，不占额外空间也不抬高 RSS
（T19 全程 RSS 33.4G / 上限 60G）。1h 间隔在 2XL64 段上付约 3% 吞吐换 1h 崩溃粒度，按此选型即可。

浸泡测试用 `--local-ckpt-seconds 600 --window-hours 0.5` 把两个周期各放大 6 / 48 倍，
上表右列已折算回生产参数；同时验证了 `kill -9` 后从 `ckpt.bin` 高度续跑。

## 依赖

- Python 3.11+（需 `boto3`，用于输出桶 multipart）
- `aws` CLI（读输入包 / 签预签名 URL；输出桶可用 `CFXRUN_OUTPUT_PROFILE`）
- `tccli`（开关腾讯云 CVM，需在 `PATH` 中或设 `CFXRUN_TCCLI`）
- SSH 私钥（与开机用的密钥对一致）
- 已构建的 Linux `cfx-replay-exec`（v3.0.3 merge）与 `label_blame`、`hydra-v303.toml`

## 本地密钥 / 路径（勿提交）

通过环境变量注入，**不要把私钥或 AWS access key 写进仓库**：

| 变量 | 含义 | 默认 |
|---|---|---|
| `CFXRUN_SSH_KEY` | SSH 私钥路径 | `~/.ssh/id_rsa` |
| `CFXRUN_AWS_REGION` | 输入/输出 S3 的 region | `ap-east-1` |
| `CFXRUN_OUTPUT_PROFILE` | 写 `conflux-replay-txs` 的 AWS profile | 空（默认 credential chain） |
| `CFXRUN_TCCLI` | `tccli` 可执行文件路径 | `PATH` 中的 `tccli` |
| `CFXRUN_TC_JSON` | 覆盖腾讯云资源字段的 JSON | 不设则用脚本内默认 VPC/镜像等 |

输入 S3 走本机 `aws` 默认 credential chain（或 `AWS_PROFILE`）。腾讯云 VPC / 子网 / 安全组 / 镜像 / 密钥 ID 写在 `cfxrun.py` 顶部，也可用 `CFXRUN_TC_JSON` 覆盖。

## 用法

```bash
export CFXRUN_SSH_KEY=~/.ssh/your_key.pem
export AWS_PROFILE=your-s3-profile
export CFXRUN_OUTPUT_PROFILE=your-s3-profile   # 可选

python3 cfxrun/cfxrun.py plan <run> [--prefix mainnet] [--only T5,T17] [--window-hours 24] [--local-ckpt-seconds 3600]
python3 cfxrun/cfxrun.py up <run> --bin /path/to/bin/cfx-replay-exec --label-blame /path/to/label_blame --config /path/to/hydra-v303.toml
python3 cfxrun/cfxrun.py status <run>
python3 cfxrun/cfxrun.py watch <run> [--interval 600]
python3 cfxrun/cfxrun.py down <run> [--all] [--discard-checkpoints]
```

产物在 `cfxrun/runs/<run>/`：`plan.json`、`state.json`、`logs/`、`watch.log`（该目录已 gitignore）。

Worker 不持有云凭据：输入用预签名 URL 下载；中间 ckpt 用主控签发的 multipart 预签名直传。
同一 epoch 范围优先 `.cfxpackb`；整段打标后 `--anomaly-streak 0`。`down`/`watch` 对成功段先上传再销机；
`FAILED label_blame`（含缺二进制）拉日志后**自动销机**；其它失败（anomaly/crash）留机，查完后 `down --all` 再销毁。
`watch` 只在所有段都是 `DONE` 时退出 0；`DONE_WITH_MISMATCH` 也按失败处理。
