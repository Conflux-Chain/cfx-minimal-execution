#!/usr/bin/env python3
"""主网/测试网并行重放验证：按 S3 上的 checkpoint 切段，一段一台腾讯云按量机器。

完整路径（缺一步 PoS 段会算叉或留下孤立 mismatch）：
  1. Linux 上 TAG=v3.0.3 ./scripts/build_linux_bins.sh
     → cfx-replay-exec（build_replay.sh merge v3.0.3，禁止裸编 oracle HEAD）
     → label_blame
  2. cfxrun.py plan / up --bin .../cfx-replay-exec-v303 --label-blame .../label_blame
     --config hydra-v303.toml
  3. worker 下载 .cfxpack 后先 label_blame 成 .cfxpackb，再 --anomaly-streak 0 重放
  4. 执行器单进程跑完整段，本地约每 1h 原地覆盖 ckpt.bin；独立计时器在墙钟超过 24h 时
     直接快照现成 ckpt.bin 压缩，由主控 multipart 上传到
     s3://conflux-replay-txs/checkpoints/<run>/<seg>/（输入侧仍只用
     cfx-minimal-execution 的作者 ckpt 切段；中间 ckpt 供以后更密并行，不自动增加本轮机器数）

  cfxrun.py plan  <run> [--prefix mainnet] [--only T5,T17] [--window-hours 24]
  cfxrun.py up    <run>            开机并部署（可重复执行，只补缺的）
  cfxrun.py status <run>           查看各段进度，异常会标 ALERT
  cfxrun.py down  <run> [--all]    上传中间 ckpt、拉回日志；成功段销机，失败段留机
  cfxrun.py watch <run> [--interval 600]   循环 status + down，写 watch.log，异常立刻打印

运行目录 runs/<run>/：plan.json、state.json、logs/、watch.log。
"""
import argparse, json, math, os, re, shlex, shutil, subprocess, sys, time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import boto3  # type: ignore[import-not-found]
from botocore.config import Config  # type: ignore[import-not-found]
from botocore.exceptions import ClientError  # type: ignore[import-not-found]

ROOT = Path(__file__).resolve().parent
SOURCE_BUCKET = "s3://cfx-minimal-execution"
OUTPUT_BUCKET = "conflux-replay-txs"
OUTPUT_PREFIX = "checkpoints"
OUTPUT_PROFILE = os.environ.get("CFXRUN_OUTPUT_PROFILE", "")
S3_PART_SIZE = 256 * 1024 * 1024
# Local secrets / machine paths stay out of the repo: set via env (or AWS default chain).
AWS_REGION = os.environ.get("CFXRUN_AWS_REGION", "ap-east-1")
# Cloud resource IDs are environment config (not credentials). Override with CFXRUN_TC_JSON if needed.
TC = dict(region="ap-hongkong", zone="ap-hongkong-2", image="img-mmytdhbn",
          vpc="vpc-qpt11awo", subnet="subnet-hpuerpcf", sg="sg-2rkmj46v", key_id="skey-9tzohzq7")
if os.environ.get("CFXRUN_TC_JSON"):
    TC.update(json.loads(os.environ["CFXRUN_TC_JSON"]))
SSH_KEY = os.path.expanduser(os.environ.get("CFXRUN_SSH_KEY", "~/.ssh/id_rsa"))
SSH = ["-i", SSH_KEY, "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
       "-o", "LogLevel=ERROR", "-o", "ConnectTimeout=15"]
TCCLI = os.environ.get("CFXRUN_TCCLI") or shutil.which("tccli") or "tccli"
TIERS = [(8, "SA9.MEDIUM8"), (16, "MA9.MEDIUM16"), (32, "MA9.LARGE32"), (64, "MA9.2XLARGE64")]
GROUP = 2000
PREP = ("DOWNLOADING", "DECOMPRESSING", "LABELLING", "ARCHIVING")
# 连续多少秒高度不前进 → 判定卡住（加载/写 ckpt 除外；status 带 DECOMPRESSING/LABELLING 不告警）
STUCK_SECS = 45 * 60
# 机型内存上限（GiB），超过 90% 告警
MEM_LIMIT = {"SA9.MEDIUM8": 7.5, "MA9.MEDIUM16": 15, "MA9.LARGE32": 30, "MA9.2XLARGE64": 60}
# label_blame 把 .cfxpack.zst 写成未压缩 .cfxpackb，盘上常驻约按此倍数估
PACK_EXPAND = 5.0


def sh(cmd, check=True, **kw):
    r = subprocess.run(cmd, capture_output=True, text=True, **kw)
    if check and r.returncode != 0:
        raise RuntimeError(f"{' '.join(cmd[:4])}...: {r.stderr.strip()}")
    return r.stdout


def tccli(action, **params):
    cmd = [TCCLI, "cvm", action, "--region", TC["region"]]
    for k, v in params.items():
        cmd += [f"--{k}", v if isinstance(v, str) else json.dumps(v)]
    return json.loads(sh(cmd))


def s3_ls(prefix):
    out = sh(["aws", "s3", "ls", f"{SOURCE_BUCKET}/{prefix}", "--region", AWS_REGION])
    return [(int(p[2]), p[3]) for p in (l.split() for l in out.splitlines()) if len(p) == 4]


PACK_NAME = re.compile(r"epochs_(\d+)_(\d+)\.(cfxpackb|cfxpack)(?:\.zst)?$")


def list_packs(prefix):
    """同一 epoch 范围优先 .cfxpackb，同后缀再优先 .zst。"""
    best = {}
    sizes = {}
    for size, name in s3_ls(f"{prefix}/cfxpack/"):
        m = PACK_NAME.match(name)
        if not m:
            continue
        lo, hi, kind = int(m[1]), int(m[2]), m[3]
        pri = (2 if kind == "cfxpackb" else 0) + (1 if name.endswith(".zst") else 0)
        key = (lo, hi)
        if key not in best or pri > best[key][0]:
            best[key] = (pri, name)
            sizes[name] = size
    packs = sorted((lo, hi, best[(lo, hi)][1]) for lo, hi in best)
    return packs, sizes


def run_dir(run):
    d = ROOT / "runs" / run
    d.mkdir(parents=True, exist_ok=True)
    return d


def load(run, name, default=None):
    p = run_dir(run) / name
    return json.loads(p.read_text()) if p.exists() else default


def save(run, name, obj):
    (run_dir(run) / name).write_text(json.dumps(obj, indent=1, ensure_ascii=False))


def log(run, msg, also_print=True):
    line = f"[{time.strftime('%F %T')}] {msg}"
    with open(run_dir(run) / "watch.log", "a") as f:
        f.write(line + "\n")
    if also_print:
        print(line, flush=True)


# ---------- plan ----------

def cmd_plan(a):
    ckpts = sorted((int(re.search(r"ckpt_(\d+)", n)[1]), size, n)
                   for size, n in s3_ls(f"{a.prefix}/cfxckpt/") if n.startswith("ckpt_"))
    packs, pack_size = list_packs(a.prefix)
    starts = [(0, 0, None)] + ckpts[:-1]
    segs = []
    for i, ((s, s_size, s_name), (e, e_size, _)) in enumerate(zip(starts, ckpts)):
        assert s % GROUP == 0 and e % GROUP == 0, (s, e)
        sel = [n for lo, hi, n in packs if hi >= s + 1 and lo <= e]
        assert sel and min(lo for lo, _, n in packs if n in sel) <= s + 1, f"T{i} 缺少起点 pack"
        assert max(hi for _, hi, n in packs if n in sel) >= e, f"T{i} 缺少终点 pack"
        rss = 3.15 + 5.19 * e_size / 1e9
        # T5 冒烟恢复峰值 6.6 GiB、8G 机仅余 0.3 GiB；额外留 2 GiB 给系统和加载峰值。
        need = rss * 1.15 + 2.0
        itype = next((t for mem, t in TIERS if mem >= need), None)
        if not itype:
            sys.exit(f"T{i} 估算需要 {need:.0f} GB，超出最大档位")
        # 原子覆盖 ckpt.bin 需两份未压缩空间；打标后 pack 按未压缩体积常驻；
        # 另预留一份压缩的 24h 中间归档。系统盘下限 100G（与 CLOUD.md 一致）。
        groups = (e - s) // GROUP
        labelled = all(".cfxpackb" in n for n in sel)
        pack_bytes = sum(pack_size[n] for n in sel)
        pack_resident = pack_bytes if labelled else pack_bytes * PACK_EXPAND
        data_gb = (e_size * 3.3 * 2 + e_size + pack_resident) / 1e9
        disk_gb = max(100, 10 * math.ceil((data_gb + 20) / 10))
        if not labelled and pack_bytes >= 4e9:
            disk_gb = max(disk_gb, 120)  # T20 一类大 pack 段
        segs.append(dict(seg=f"T{i}", start=s, end=e, groups=groups,
                         window_seconds=round(a.window_hours * 3600),
                         local_ckpt_seconds=a.local_ckpt_seconds,
                         anomaly_streak=0 if labelled else 20,
                         ckpt=f"{a.prefix}/cfxckpt/{s_name}" if s_name else None,
                         packs=[f"{a.prefix}/cfxpack/{n}" for n in sel],
                         labelled=labelled,
                         est_rss_gb=round(rss, 1), instance_type=itype,
                         disk_gb=disk_gb))
    if a.only:
        keep = set(a.only.split(","))
        segs = [x for x in segs if x["seg"] in keep]
    save(a.run, "plan.json", dict(
        prefix=a.prefix,
        output=f"s3://{OUTPUT_BUCKET}/{OUTPUT_PREFIX}/{a.run}/",
        window_hours=a.window_hours,
        segments=segs))
    for x in segs:
        tag = "cfxpackb" if x["labelled"] else "cfxpack"
        print(f"{x['seg']:>4} {x['start']:>11,} → {x['end']:>11,}  groups={x['groups']:>6}  "
              f"packs={len(x['packs']):>3} {tag}  streak={x['anomaly_streak']}  "
              f"估RSS={x['est_rss_gb']:>5}G  {x['instance_type']:<14} 盘{x['disk_gb']}G")
    print(f"共 {len(segs)} 段 → runs/{a.run}/plan.json")
    print(f"中间 ckpt 输出前缀 → s3://{OUTPUT_BUCKET}/{OUTPUT_PREFIX}/{a.run}/"
          f"（仅墙钟≥{a.window_hours:g}h 的段会上传；不改变本轮切段数）")


# ---------- up ----------

def launch(run, x):
    r = tccli("RunInstances", InstanceChargeType="POSTPAID_BY_HOUR",
              Placement={"Zone": TC["zone"]}, InstanceType=x["instance_type"], ImageId=TC["image"],
              SystemDisk={"DiskType": "CLOUD_PREMIUM", "DiskSize": x["disk_gb"]},
              VirtualPrivateCloud={"VpcId": TC["vpc"], "SubnetId": TC["subnet"]},
              SecurityGroupIds=[TC["sg"]], LoginSettings={"KeyIds": [TC["key_id"]]},
              InternetAccessible={"InternetChargeType": "TRAFFIC_POSTPAID_BY_HOUR",
                                  "InternetMaxBandwidthOut": 100, "PublicIpAssigned": True},
              InstanceName=f"cfx-{run}-{x['seg']}")
    return r["InstanceIdSet"][0]


def presign(key):
    return sh(["aws", "s3", "presign", f"{SOURCE_BUCKET}/{key}", "--region", AWS_REGION,
               "--expires-in", "43200"]).strip()


def output_s3():
    profile = OUTPUT_PROFILE or None
    return boto3.Session(profile_name=profile, region_name=AWS_REGION).client(
        "s3", endpoint_url=f"https://s3.{AWS_REGION}.amazonaws.com", config=Config(
            retries={"total_max_attempts": 3, "mode": "standard"},
            connect_timeout=5, read_timeout=30))


def check_output_s3(run):
    """开机前只读确认输出 bucket 可访问；运行代码绝不删除 S3 对象。"""
    del run
    output_s3().head_bucket(Bucket=OUTPUT_BUCKET)


def deploy(run, x, ip, a):
    host = f"ubuntu@{ip}"
    for _ in range(20):
        if subprocess.run(["ssh", *SSH, host, "mkdir -p ~/w"], capture_output=True).returncode == 0:
            break
        time.sleep(10)
    else:
        raise RuntimeError(f"{x['seg']} ssh 不通")
    stage = run_dir(run) / "stage" / x["seg"]
    stage.mkdir(parents=True, exist_ok=True)
    (stage / "seg.env").write_text(
        f"SEG={x['seg']}\nSTART={x['start']}\nEND={x['end']}\n"
        f"WINDOW_SECONDS={x['window_seconds']}\n"
        f"LOCAL_CKPT_SECONDS={x.get('local_ckpt_seconds', 3600)}\n"
        f"ANOMALY_STREAK={x.get('anomaly_streak', 0)}\n"
        f"LABEL_WINDOW=10000\n")
    lines = [f"input/{Path(k).name} {presign(k)}" for k in x["packs"]]
    if x["ckpt"]:
        lines.append(f"ckpt.bin.zst {presign(x['ckpt'])}")
    (stage / "urls.txt").write_text("\n".join(lines) + "\n")
    sh(["scp", "-q", *SSH, a.bin, f"{host}:w/cfx-replay-exec"])
    sh(["scp", "-q", *SSH, a.config, f"{host}:w/hydra.toml"])
    extras = [str(ROOT / "worker.sh"), str(stage / "seg.env"), str(stage / "urls.txt")]
    label = getattr(a, "label_blame", None)
    if label and Path(label).is_file():
        extras.append(label)
    sh(["scp", "-q", *SSH, *extras, f"{host}:w/"])
    chmod = "chmod +x cfx-replay-exec worker.sh"
    if label and Path(label).is_file():
        chmod += " label_blame"
    sh(["ssh", *SSH, host, f"cd ~/w && {chmod} && "
        "(setsid nohup ./worker.sh > worker.out 2>&1 < /dev/null &)"])


def cmd_up(a):
    plan, state = load(a.run, "plan.json"), load(a.run, "state.json", {})
    need_label = any(not x.get("labelled") for x in plan["segments"])
    if need_label and not Path(getattr(a, "label_blame", "") or "").is_file():
        sys.exit("完整路径需要 Linux label_blame：S3 上仍是 .cfxpack。"
                 "在 Linux 上跑 ./scripts/build_linux_bins.sh，up 时 --label-blame <path>")
    check_output_s3(a.run)
    log(a.run, f"输出 S3 预检通过：s3://{OUTPUT_BUCKET}/{OUTPUT_PREFIX}/{a.run}/")
    log(a.run, f"up 开始，共 {len(plan['segments'])} 段")
    for x in plan["segments"]:
        s = state.setdefault(x["seg"], {})
        if not s.get("id"):
            s["id"] = launch(a.run, x)
            log(a.run, f"{x['seg']}: 已创建 {s['id']} ({x['instance_type']})")
            save(a.run, "state.json", state)
    pending = {x["seg"]: x for x in plan["segments"] if not state[x["seg"]].get("deployed")}
    while pending:
        ids = [state[k]["id"] for k in pending]
        for i in tccli("DescribeInstances", InstanceIds=ids, Limit=100)["InstanceSet"]:
            seg = next(k for k in pending if state[k]["id"] == i["InstanceId"])
            if i["InstanceState"] == "RUNNING" and i.get("PublicIpAddresses"):
                state[seg]["ip"] = i["PublicIpAddresses"][0]
        ready = [k for k in pending if state[k].get("ip")]
        with ThreadPoolExecutor(8) as ex:
            futs = {k: ex.submit(deploy, a.run, pending[k], state[k]["ip"], a) for k in ready}
        for k, f in futs.items():
            try:
                f.result()
                state[k]["deployed"] = True
                pending.pop(k)
                log(a.run, f"{k}: 已部署 {state[k]['ip']}，worker 已启动")
            except Exception as e:
                log(a.run, f"ALERT {k}: 部署失败，稍后重试：{e}")
        save(a.run, "state.json", state)
        if pending:
            log(a.run, f"等待开机/部署：{sorted(pending)}")
            time.sleep(15)
    log(a.run, "up 完成，全部已部署")


# ---------- status / down ----------

PROG = re.compile(
    r"replay progress t=(\d+)s height=(\d+).*?epochs=(\d+).*?"
    r"state_match=(\d+)/(\d+) longest_mismatch_run=(\d+)")


def probe(x, s):
    """返回 dict：status / note / alerts / height / pct / eta_h / eps / rss_g"""
    info = dict(status="UNKNOWN", note="", alerts=[], height=None, pct=None, eta_h=None,
                eps=None, rss_g=None, cpu_pct=None, disk_pct=None, disk_free_g=None,
                mem_avail_g=None, archive_g=None, archive_count=0,
                replay_alive=False, worker_alive=False)
    if s.get("terminated"):
        info["status"] = s.get("final", "TERMINATED")
        return info
    if not s.get("ip"):
        info["status"] = "NO_IP"
        info["alerts"].append("尚未拿到公网 IP")
        return info
    try:
        out = subprocess.run(
            ["ssh", *SSH, f"ubuntu@{s['ip']}",
             "cd ~/w; "
             "printf 'STATUS\\t'; cat status 2>/dev/null || echo NO_STATUS; "
             "p=$(grep 'replay progress' run.log 2>/dev/null | tail -1); "
             "[ -z \"$p\" ] || printf 'PROGRESS\\t%s\\n' \"$p\"; "
             "pid=$(pgrep -n -x cfx-replay-exec 2>/dev/null || true); "
             "if [ -n \"$pid\" ]; then "
             "  ps -p \"$pid\" -o pid=,rss=,%cpu=,etime= | "
             "    awk '{print \"PROC\\t\"$1\"\\t\"$2\"\\t\"$3\"\\t\"$4}'; "
             "else echo 'PROC\tNONE'; fi; "
             "df -Pk / | awk 'NR==2 {gsub(\"%\", \"\", $5); print \"DISK\\t\"$5\"\\t\"$4}'; "
             "free -k | awk '/Mem:/ {print \"MEM\\t\"$2\"\\t\"$3\"\\t\"$7}'; "
             "akb=$(du -sk archives 2>/dev/null | awk '{print $1}'); akb=${akb:-0}; "
             "if [ -f archives/manifest.tsv ]; then ac=$(wc -l < archives/manifest.tsv); else ac=0; fi; "
             "printf 'ARCHIVES\\t%s\\t%s\\n' \"$ac\" \"$akb\"; "
             "alive=0; "
             "if [ -s worker.pid ] && kill -0 \"$(cat worker.pid)\" 2>/dev/null; then alive=1; "
             "elif pgrep -f '[.]\\/worker.sh' >/dev/null 2>&1; then alive=1; fi; "
             "printf 'WORKER\\t%s\\n' \"$alive\"; "
             "e=$(grep -Ei 'panic|fatal|out of memory|killed process' run.log 2>/dev/null | tail -1); "
             "[ -z \"$e\" ] || printf 'ERROR\\t%s\\n' \"$e\"; "
             "mm=$(grep '^epoch height=' run.log 2>/dev/null | tail -1); "
             "[ -z \"$mm\" ] || printf 'MISMATCH\\t%s\\n' \"$mm\""],
            capture_output=True, text=True, timeout=30)
    except subprocess.TimeoutExpired:
        info["status"] = "UNREACHABLE"
        info["alerts"].append("SSH 探测超时")
        return info
    if out.returncode != 0 and not out.stdout.strip():
        info["status"] = "UNREACHABLE"
        info["alerts"].append("SSH 不通")
        return info
    fields = {}
    for line in out.stdout.splitlines():
        key, sep, value = line.partition("\t")
        if sep:
            fields[key] = value.strip()
    status = fields.get("STATUS", "NO_STATUS")
    info["status"] = status

    m = PROG.search(fields.get("PROGRESS", ""))
    if m:
        t, h, ep, ok, tot, run_ = map(int, m.groups())
        status_parts = status.split()
        status_height = (int(status_parts[1]) if len(status_parts) > 1
                         and status_parts[0] == "RUNNING" and status_parts[1].isdigit()
                         else None)
        # 刚恢复下一窗口时 run.log 末行仍属于上一窗口，不用它计算当前 ETA。
        if status_height is not None and h < status_height:
            m = None
            info["height"] = status_height
            info["pct"] = ((status_height - x["start"]) * 100
                           / max(x["end"] - x["start"], 1))
            info["note"] = f"{info['pct']:5.1f}% 高度≈{status_height:,} 等待新窗口心跳"
    if m:
        t, h, ep, ok, tot, run_ = map(int, m.groups())
        eps = ep / max(t, 1)
        eta = (x["end"] - h) / eps / 3600 if eps > 0 else None
        pct = (h - x["start"]) * 100 / max(x["end"] - x["start"], 1)
        info.update(height=h, pct=pct, eta_h=eta, eps=eps)
        info["note"] = (f"{pct:5.1f}% 高度={h:,} {eps:.0f}eps "
                        f"剩余≈{eta:.1f}h 匹配={ok}/{tot}" if eta is not None else
                        f"{pct:5.1f}% 高度={h:,} 匹配={ok}/{tot}")
        if run_ > 0:
            info["alerts"].append(f"连续不匹配={run_}")
        if ok < tot:
            info["alerts"].append(f"本块有不匹配 {tot - ok}/{tot}")

    proc = fields.get("PROC", "NONE").split("\t")
    if proc[0] != "NONE" and len(proc) == 4:
        info["replay_alive"] = True
        info["rss_g"] = int(proc[1]) / 1048576
        info["cpu_pct"] = float(proc[2])
        info["note"] += f" CPU={info['cpu_pct']:.0f}% RSS={info['rss_g']:.1f}G"
        lim = MEM_LIMIT.get(x["instance_type"])
        if lim and info["rss_g"] > lim * 0.9:
            info["alerts"].append(f"内存接近上限 {info['rss_g']:.1f}/{lim}G")

    disk = fields.get("DISK", "").split("\t")
    if len(disk) == 2 and all(v.isdigit() for v in disk):
        info["disk_pct"] = int(disk[0])
        info["disk_free_g"] = int(disk[1]) / 1048576
        info["note"] += f" 盘={info['disk_pct']}%/余{info['disk_free_g']:.1f}G"
        if info["disk_pct"] >= 90 or info["disk_free_g"] < 5:
            info["alerts"].append(
                f"磁盘空间不足：占用 {info['disk_pct']}%，剩余 {info['disk_free_g']:.1f}G")

    mem = fields.get("MEM", "").split("\t")
    if len(mem) == 3 and all(v.isdigit() for v in mem):
        info["mem_avail_g"] = int(mem[2]) / 1048576
        info["note"] += f" MemAvail={info['mem_avail_g']:.1f}G"
        if info["mem_avail_g"] < 0.75:
            info["alerts"].append(f"系统可用内存仅 {info['mem_avail_g']:.1f}G")

    archives = fields.get("ARCHIVES", "").split("\t")
    if len(archives) == 2 and all(v.isdigit() for v in archives):
        info["archive_count"] = int(archives[0])
        info["archive_g"] = int(archives[1]) / 1048576
        info["note"] += f" 归档={info['archive_count']}/{info['archive_g']:.1f}G"

    info["worker_alive"] = fields.get("WORKER") == "1"
    active = status.startswith(("RUNNING",) + PREP)
    if active and not info["worker_alive"]:
        info["alerts"].append("worker.sh 已退出但状态仍显示运行中")
    if status.startswith("RUNNING") and not info["replay_alive"]:
        info["alerts"].append("RUNNING 状态下 cfx-replay-exec 进程不存在")

    remote_error = fields.get("ERROR")
    if remote_error and remote_error != s.get("last_remote_error"):
        info["alerts"].append(f"run.log 异常：{remote_error}")
        s["last_remote_error"] = remote_error
    mismatch = fields.get("MISMATCH")
    if mismatch and mismatch != s.get("last_mismatch"):
        info["alerts"].append(f"发现 epoch mismatch：{mismatch}")
        s["last_mismatch"] = mismatch

    if status.startswith("FAILED"):
        info["alerts"].append(status)
    if status.startswith("DONE_WITH_MISMATCH"):
        info["alerts"].append(status)
    known = ("RUNNING",) + PREP + ("DONE", "FAILED")
    if not status.startswith(known):
        info["alerts"].append(f"未知状态：{status}")
    return info


def cmd_status(a, quiet=False):
    plan, state = load(a.run, "plan.json"), load(a.run, "state.json", {})
    now = time.time()
    rows = []
    with ThreadPoolExecutor(16) as ex:
        futs = {x["seg"]: ex.submit(probe, x, state.get(x["seg"], {}))
                for x in plan["segments"] if x["seg"] in state}
    alerts = []
    done = running = failed = other = 0
    eta_max = 0.0
    for x in plan["segments"]:
        if x["seg"] not in futs:
            continue
        info = futs[x["seg"]].result()
        s = state[x["seg"]]
        # 卡住检测：高度长时间不变
        h = info.get("height")
        if h is not None and not str(info["status"]).startswith(("DONE", "FAILED", "TERMINATED")):
            s.pop("no_progress_since", None)
            if s.get("last_height") == h:
                stuck = now - s.get("last_height_ts", now)
                if stuck > STUCK_SECS and not str(info["status"]).startswith(PREP):
                    info["alerts"].append(f"卡住 {stuck / 60:.0f} 分钟高度未前进")
            else:
                s["last_height"] = h
                s["last_height_ts"] = now
        elif str(info["status"]).startswith("RUNNING"):
            no_progress = s.setdefault("no_progress_since", now)
            if now - no_progress > 5 * 60:
                info["alerts"].append("RUNNING 超过 5 分钟仍没有 progress 心跳")
        elif str(info["status"]).startswith(("RUNNING",) + PREP):
            s.setdefault("last_height_ts", now)

        st = info["status"]
        if st.startswith("DONE") or s.get("terminated"):
            done += 1
        elif st.startswith("FAILED"):
            failed += 1
        elif st.startswith(("RUNNING",) + PREP):
            running += 1
            if info.get("eta_h"):
                eta_max = max(eta_max, info["eta_h"])
        else:
            other += 1

        mark = "⚠" if info["alerts"] else " "
        line = f"{mark}{x['seg']:>3} {s.get('ip', '-'):>15}  {st:<28} {info['note']}"
        rows.append((x["seg"], st, info))
        if not quiet:
            print(line, flush=True)
            for al in info["alerts"]:
                alerts.append(f"{x['seg']}: {al}")
                print(f"   !! ALERT {x['seg']}: {al}", flush=True)

    save(a.run, "state.json", state)
    total = len(rows)
    summary = (f"汇总：完成 {done}/{total}  运行中 {running}  失败 {failed}  其他 {other}"
               + (f"  最慢段剩余≈{eta_max:.1f}h" if eta_max else ""))
    if not quiet:
        print(summary, flush=True)
    return rows, alerts, summary


def pull_logs(seg, s, logs):
    if not s.get("ip"):
        return
    for f in ("run.log", "status", "worker.out"):
        subprocess.run(["scp", "-q", *SSH, f"ubuntu@{s['ip']}:w/{f}", str(logs / f"{seg}.{f}")],
                       capture_output=True)


def remote_checkpoint_manifest(s):
    """读取已完整压缩的 checkpoint 清单和正在压缩的高度。"""
    out = subprocess.run(
        ["ssh", *SSH, f"ubuntu@{s['ip']}",
         "cd ~/w && (cat archives/manifest.tsv 2>/dev/null || true); "
         "echo __PENDING__; cat pending_archive 2>/dev/null || true"],
        capture_output=True, text=True, timeout=30)
    if out.returncode != 0:
        raise RuntimeError(f"读取远端 checkpoint 清单失败：{out.stderr.strip()}")
    manifest_text, marker, pending = out.stdout.partition("__PENDING__\n")
    if not marker:
        raise RuntimeError("远端 checkpoint 清单格式错误")
    items = []
    for line in manifest_text.splitlines():
        if not line.strip():
            continue
        parts = line.split("\t")
        if len(parts) != 4:
            raise RuntimeError(f"远端 checkpoint 清单行格式错误：{line!r}")
        height, size, sha256, filename = parts
        if not (height.isdigit() and size.isdigit()
                and re.fullmatch(r"[0-9a-f]{64}", sha256)
                and filename == f"ckpt_{height}.bin.zst"):
            raise RuntimeError(f"远端 checkpoint 清单内容非法：{line!r}")
        items.append(dict(height=int(height), size=int(size), sha256=sha256,
                          filename=filename))
    return sorted(items, key=lambda x: x["height"]), pending.strip()


def upload_checkpoint(a, seg, s, state, item):
    """由本地签发 multipart URL，worker 直传 S3；凭据始终只留在本地。"""
    height, size, sha256 = item["height"], item["size"], item["sha256"]
    key = f"{OUTPUT_PREFIX}/{a.run}/{seg}/{item['filename']}"
    old = s.setdefault("uploaded_checkpoints", {}).get(str(height))
    if old and old.get("size") == size and old.get("sha256") == sha256:
        return

    s3 = output_s3()
    try:
        head = s3.head_object(Bucket=OUTPUT_BUCKET, Key=key)
    except ClientError as e:
        code = e.response.get("Error", {}).get("Code")
        if code not in ("404", "NoSuchKey", "NotFound"):
            raise
    else:
        if (head.get("ContentLength") == size
                and head.get("Metadata", {}).get("sha256") == sha256):
            s["uploaded_checkpoints"][str(height)] = dict(
                key=key, size=size, sha256=sha256, uploaded_at=int(time.time()))
            save(a.run, "state.json", state)
            return
        raise RuntimeError(f"S3 已存在同名但内容不同的对象：s3://{OUTPUT_BUCKET}/{key}")

    uploads = s.setdefault("multipart_uploads", {})
    record = uploads.get(str(height))
    if not record:
        created = s3.create_multipart_upload(
            Bucket=OUTPUT_BUCKET, Key=key, ServerSideEncryption="AES256",
            Metadata={"sha256": sha256, "height": str(height), "segment": seg})
        record = dict(key=key, upload_id=created["UploadId"], size=size,
                      sha256=sha256, part_size=S3_PART_SIZE, parts={},
                      created_at=int(time.time()))
        uploads[str(height)] = record
        save(a.run, "state.json", state)
    elif (record.get("key") != key or record.get("size") != size
          or record.get("sha256") != sha256):
        raise RuntimeError(f"已有 multipart 状态与 checkpoint {height} 不一致")

    upload_id = record["upload_id"]
    completed = {}
    paginator = s3.get_paginator("list_parts")
    for page in paginator.paginate(Bucket=OUTPUT_BUCKET, Key=key, UploadId=upload_id):
        for part in page.get("Parts", []):
            n = part["PartNumber"]
            expected_size = min(S3_PART_SIZE, size - (n - 1) * S3_PART_SIZE)
            if part.get("Size") == expected_size:
                completed[n] = part["ETag"]

    part_count = math.ceil(size / S3_PART_SIZE)
    log(a.run, f"{seg}: worker 直传 checkpoint {height:,}（{size / 1e9:.2f} GB，"
        f"{part_count} parts）→ s3://{OUTPUT_BUCKET}/{key}")
    remote_file = f"/home/ubuntu/w/archives/{item['filename']}"
    for part_number in range(1, part_count + 1):
        if part_number in completed:
            continue
        offset = (part_number - 1) * S3_PART_SIZE
        length = min(S3_PART_SIZE, size - offset)
        url = s3.generate_presigned_url(
            "upload_part",
            Params={"Bucket": OUTPUT_BUCKET, "Key": key, "UploadId": upload_id,
                    "PartNumber": part_number},
            ExpiresIn=3600)
        pipe = (
            "tmp=$(mktemp); trap 'rm -f \"$tmp\"' EXIT; "
            f"dd if={shlex.quote(remote_file)} of=\"$tmp\" "
            f"iflag=skip_bytes,count_bytes skip={offset} count={length} status=none; "
            f"curl --fail-with-body -sS --upload-file \"$tmp\" -D - -o - {shlex.quote(url)}"
        )
        sent = subprocess.run(
            ["ssh", *SSH, f"ubuntu@{s['ip']}",
             f"bash -o pipefail -c {shlex.quote(pipe)}"],
            capture_output=True, text=True, timeout=7200)
        if sent.returncode != 0:
            raise RuntimeError(
                f"part {part_number}/{part_count} 上传失败："
                f"{(sent.stderr.strip() or sent.stdout[-1000:])}")
        match = re.search(r'(?im)^etag:\s*("?[0-9a-f]+"?)\s*$', sent.stdout)
        if not match:
            raise RuntimeError(
                f"part {part_number}/{part_count} 未返回 ETag：{sent.stdout[-500:]}")
        completed[part_number] = match.group(1)
        record["parts"][str(part_number)] = completed[part_number]
        save(a.run, "state.json", state)
        log(a.run, f"{seg}: checkpoint {height:,} part {part_number}/{part_count} 上传成功")

    parts = [{"PartNumber": n, "ETag": completed[n]} for n in range(1, part_count + 1)]
    s3.complete_multipart_upload(
        Bucket=OUTPUT_BUCKET, Key=key, UploadId=upload_id,
        MultipartUpload={"Parts": parts})
    head = s3.head_object(Bucket=OUTPUT_BUCKET, Key=key)
    if head.get("ContentLength") != size or head.get("Metadata", {}).get("sha256") != sha256:
        raise RuntimeError(f"checkpoint 上传校验失败：height={height}")
    record["completed_at"] = int(time.time())
    s["uploaded_checkpoints"][str(height)] = dict(
        key=key, size=size, sha256=sha256, uploaded_at=int(time.time()))
    save(a.run, "state.json", state)
    log(a.run, f"{seg}: checkpoint {height:,} 上传并校验成功")


def preserve_checkpoints(a, seg, st, s, state, require_complete=True):
    items, pending = remote_checkpoint_manifest(s)
    if require_complete and pending:
        raise RuntimeError(f"高度 {pending} 的 checkpoint 尚未压缩完成")
    expected = None
    parts = st.split()
    if st.startswith("DONE_PENDING_UPLOAD") and len(parts) >= 3:
        expected = int(parts[2])
    else:
        m = re.search(r"\bPENDING_UPLOAD\s+(\d+)", st)
        if m:
            expected = int(m.group(1))
    if expected is not None and len(items) != expected:
        raise RuntimeError(f"远端声称有 {expected} 个 checkpoint，清单实际为 {len(items)} 个")
    for item in items:
        upload_checkpoint(a, seg, s, state, item)
    if require_complete:
        s["checkpoints_preserved"] = True
        save(a.run, "state.json", state)


def cmd_down(a, from_watch=False):
    state = load(a.run, "state.json", {})
    logs = run_dir(a.run) / "logs"
    logs.mkdir(exist_ok=True)
    rows, alerts, summary = cmd_status(a, quiet=from_watch)
    if from_watch:
        for seg, st, info in rows:
            mark = "⚠" if info["alerts"] else " "
            log(a.run, f"{mark}{seg:>3} {state[seg].get('ip', '-'):>15}  {st:<28} {info['note']}")
            for al in info["alerts"]:
                log(a.run, f"   !! ALERT {seg}: {al}")
        log(a.run, summary)

    for seg, st, info in rows:
        s = state[seg]
        if s.get("terminated"):
            continue
        finishing = a.all or st.startswith(("DONE", "FAILED"))
        # 运行期间也及时上传已压缩完成的 24h 边界；主控离线时文件仍安全留在 worker。
        if not finishing and s.get("ip") and not a.discard_checkpoints:
            try:
                preserve_checkpoints(a, seg, st, s, state, require_complete=False)
            except Exception as e:
                msg = f"{seg}: checkpoint 后台持久化失败：{e}"
                alerts.append(msg)
                log(a.run, f"ALERT {msg}")
            continue
        if not finishing:
            continue
        # label_blame 失败是确定性的，日志里已有原因；留机只烧钱，拉日志后直接销。
        # 其它 FAILED（anomaly/crash 等）仍留机排查。
        label_fail = st.startswith("FAILED") and "label_blame" in st
        keep_failed = st.startswith("FAILED") and not a.all and not label_fail
        if keep_failed:
            if not s.get("kept_for_debug"):
                pull_logs(seg, s, logs)
                s["kept_for_debug"] = True
                s["final"] = st
                save(a.run, "state.json", state)
                log(a.run, f"ALERT {seg}: 失败留机排查（{st}），ckpt.bin 在 worker 上，"
                    f"日志 → {logs}/{seg}.*；查完后 down --all 再销")
            continue
        if s.get("ip"):
            pull_logs(seg, s, logs)
        if label_fail:
            log(a.run, f"ALERT {seg}: label_blame 失败，拉日志后销机（{st}）")
        elif a.discard_checkpoints:
            log(a.run, f"ALERT {seg}: 按 --discard-checkpoints 明确放弃中间 checkpoint")
        elif s.get("ip"):
            try:
                preserve_checkpoints(a, seg, st, s, state)
            except Exception as e:
                msg = f"{seg}: checkpoint 未安全持久化，拒绝销机：{e}"
                alerts.append(msg)
                log(a.run, f"ALERT {msg}")
                continue
        elif st.startswith("DONE") and "PENDING_UPLOAD" in st:
            msg = f"{seg}: 没有 IP，无法确认 checkpoint 已持久化，拒绝销机"
            alerts.append(msg)
            log(a.run, f"ALERT {msg}")
            continue
        if not s.get("id"):
            log(a.run, f"ALERT {seg}: 没有实例 ID，拒绝销机")
            continue
        try:
            tccli("TerminateInstances", InstanceIds=[s["id"]])
        except Exception as e:
            log(a.run, f"ALERT {seg}: 销毁失败 {e}")
            continue
        s.update(terminated=True, final=st)
        save(a.run, "state.json", state)
        if label_fail:
            kind = "label_blame 失败销机"
        elif a.all and not st.startswith("DONE"):
            kind = "强制销毁"
        else:
            kind = "完成"
        log(a.run, f"{seg}: {kind}（{st}），日志 → {logs}/{seg}.*")

    left = [seg for seg, st, _ in rows
            if not state[seg].get("terminated") and not st.startswith("FAILED")]
    kept = [seg for seg, st, _ in rows
            if not state[seg].get("terminated") and st.startswith("FAILED")
            and "label_blame" not in st]
    if not from_watch:
        print(f"仍在运行：{len(left)} 台 {left}", flush=True)
        if kept:
            print(f"失败留机：{len(kept)} 台 {kept}", flush=True)
    return left, alerts


def cmd_watch(a):
    log(a.run, f"watch 开始，间隔 {a.interval}s，日志 → runs/{a.run}/watch.log")
    round_n = 0
    while True:
        round_n += 1
        log(a.run, f"======== 第 {round_n} 轮 ========")
        left, alerts = cmd_down(a, from_watch=True)
        if alerts:
            log(a.run, f"本轮告警 {len(alerts)} 条：{'; '.join(alerts)}")
        if not left:
            break
        log(a.run, f"仍在运行 {len(left)} 台，{a.interval}s 后下一轮")
        time.sleep(a.interval)
    report = {k: s.get("final") for k, s in load(a.run, "state.json").items()}
    failed = {k: v for k, v in report.items() if not str(v).startswith("DONE")}
    log(a.run, "全部结束：" + json.dumps(report, ensure_ascii=False))
    if failed:
        log(a.run, f"ALERT 有失败或异常段：{failed}")
        sys.exit(1)
    log(a.run, "全部段验证通过")


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sp = p.add_subparsers(dest="cmd", required=True)
    for name in ("plan", "up", "status", "down", "watch"):
        q = sp.add_parser(name)
        q.add_argument("run")
        if name == "plan":
            q.add_argument("--prefix", default="mainnet")
            q.add_argument("--only", help="只跑这些段，如 T5,T17")
            q.add_argument("--window-hours", type=float, default=24,
                           help="中间 checkpoint 墙钟窗口（小时，默认 24）；"
                                "超时则归档本地 ckpt 并上传 S3")
            q.add_argument("--local-ckpt-seconds", type=int, default=3600,
                           help="执行器覆盖本地 ckpt.bin 的间隔（秒，默认 3600）；"
                                "只影响崩溃续跑粒度，不触发上传")
            q.add_argument("--chunks", type=int,
                           help="兼容旧参数，已忽略")
        if name == "up":
            q.add_argument("--bin", default=str(
                next((c for c in (
                    ROOT.parent / "bin/cfx-replay-exec-v303",
                    ROOT.parent / "bin/cfx-replay-exec",
                    ROOT.parent / "target/release/cfx-replay-exec",
                ) if c.exists()), ROOT.parent / "bin/cfx-replay-exec-v303")),
                help="必须是 build_replay.sh merge v3.0.3 产物，禁止裸编 oracle HEAD")
            q.add_argument("--config", default=str(
                next((c for c in (
                    ROOT.parent / "data/conflux-rust-config/hydra-v303.toml",
                    ROOT.parent / "bin/hydra-v303.toml",
                    ROOT.parent / "oracle/conflux-rust/run/hydra.toml",
                ) if c.exists()), ROOT.parent / "data/conflux-rust-config/hydra-v303.toml")))
            q.add_argument("--label-blame", default=str(
                next((c for c in (
                    ROOT.parent / "bin/label_blame",
                    ROOT.parent / "target/release/label_blame",
                ) if c.exists()), ROOT.parent / "bin/label_blame")),
                help="Linux label_blame；S3 仍是 .cfxpack 时 worker 打标成 .cfxpackb")
        if name in ("down", "watch"):
            q.add_argument("--all", action="store_true",
                           help="连运行中和失败留机一并销毁")
            q.add_argument("--discard-checkpoints", action="store_true",
                           help="明确放弃未上传的中间 checkpoint 后销机（危险）")
        if name == "watch":
            q.add_argument("--interval", type=int, default=600)
    a = p.parse_args()
    globals()[f"cmd_{a.cmd}"](a)


if __name__ == "__main__":
    main()
