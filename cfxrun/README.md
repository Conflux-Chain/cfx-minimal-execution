# cfxrun — 云上分段并行重放

把主网（或测试网）按 S3 checkpoint 切成多段，每段一台腾讯云按量实例并行跑 `cfx-replay-exec`，并按约一天墙钟窗口归档中间 checkpoint 到输出 S3。

## 依赖

- Python 3.11+：`boto3`、`botocore`
- `aws` CLI（读输入包 / 签预签名 URL；输出上传用 profile）
- `tccli`（开关腾讯云 CVM）
- SSH 私钥（与开机用的密钥对一致）
- 已构建的 Linux `cfx-replay-exec` 与对应 `hydra.toml`

云 VPC / 镜像 / 密钥、S3 bucket 与 profile 等写在 `cfxrun.py` 顶部常量，按环境修改。

## 用法

```bash
# 切段计划（可只选部分段）
python3 cfxrun/cfxrun.py plan <run> [--prefix mainnet] [--only T5,T17] [--window-hours 20]

# 开机部署（需再次确认后再跑全量）
python3 cfxrun/cfxrun.py up <run> --bin /path/to/cfx-replay-exec --config /path/to/hydra.toml

python3 cfxrun/cfxrun.py status <run>
python3 cfxrun/cfxrun.py watch <run> [--interval 600]
python3 cfxrun/cfxrun.py down <run> [--all]
```

产物在 `cfxrun/runs/<run>/`：`plan.json`、`state.json`、`logs/`、`watch.log`（该目录已 gitignore）。

Worker 不持有云凭据：输入用预签名 URL 下载；输出 checkpoint 由本机签发 multipart 预签名、worker 直传，校验通过后才销机。
