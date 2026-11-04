# cfxrun — 云上分段并行重放

把主网（或测试网）按 S3 checkpoint 切成多段，每段一台腾讯云按量实例并行跑 `cfx-replay-exec`，并按约一天墙钟窗口归档中间 checkpoint 到输出 S3。

## 依赖

- Python 3.11+：`boto3`、`botocore`
- `aws` CLI（读输入包 / 签预签名 URL）
- `tccli`（开关腾讯云 CVM，需在 `PATH` 中或设 `CFXRUN_TCCLI`）
- SSH 私钥（与开机用的密钥对一致）
- 已构建的 Linux `cfx-replay-exec` 与对应 `hydra.toml`

## 本地密钥 / 路径（勿提交）

通过环境变量注入，**不要把私钥或 AWS access key 写进仓库**：

| 变量 | 含义 | 默认 |
|---|---|---|
| `CFXRUN_SSH_KEY` | SSH 私钥路径 | `~/.ssh/id_rsa` |
| `CFXRUN_AWS_PROFILE` | 输出 S3 用的 AWS named profile | 空（走默认 credential chain） |
| `CFXRUN_AWS_REGION` | AWS region | `ap-east-1` |
| `CFXRUN_TCCLI` | `tccli` 可执行文件路径 | `PATH` 中的 `tccli` |
| `CFXRUN_TC_JSON` | 覆盖腾讯云资源字段的 JSON | 不设则用脚本内默认 VPC/镜像等 |

腾讯云 VPC / 子网 / 安全组 / 镜像 / 密钥 ID，以及 S3 bucket 名，属于可分享的环境配置，默认写在 `cfxrun.py` 顶部，也可用 `CFXRUN_TC_JSON` 覆盖。

## 用法

```bash
export CFXRUN_SSH_KEY=~/.ssh/your_key.pem
export CFXRUN_AWS_PROFILE=your-s3-profile   # 可选

python3 cfxrun/cfxrun.py plan <run> [--prefix mainnet] [--only T5,T17] [--window-hours 20]
python3 cfxrun/cfxrun.py up <run> --bin /path/to/cfx-replay-exec --config /path/to/hydra.toml
python3 cfxrun/cfxrun.py status <run>
python3 cfxrun/cfxrun.py watch <run> [--interval 600]
python3 cfxrun/cfxrun.py down <run> [--all]
```

产物在 `cfxrun/runs/<run>/`：`plan.json`、`state.json`、`logs/`、`watch.log`（该目录已 gitignore）。

Worker 不持有云凭据：输入用预签名 URL 下载；输出 checkpoint 由本机签发 multipart 预签名、worker 直传，校验通过后才销机。
