本文件只允许用户直接编辑，禁止编程工具修改！


Conflux 存储特点
1. 分为 snapshot intermediate delta 三个 trie，执行时只修改 delta trie
2. 每 2000 个区块是一个 epoch，更新规则是：
    a) epoch N 结束 snapshot(N), intermediate(N) delta(N)
    b) epoch N+1 开始 snapshot(N+1) = merge(snapshot(N), intermediate(N)) intermediate(N+1)=delta(N) delta(N+1)=[]

请注意这个设计的特点
1. snapshot 在绝大多数情况是只读的，只有 merge(snapshot, intermediate) -> new snapshot 是写操作
2. 如果 2000-4000 生成的 delta mpt, 在 4000 变成 intermediate, 6000 开始的 snapshot 是 merge(4000 开始的 snapshot, 4000 开始的 intermediate)，也就是说 6000 开始的 snapshot 提前 2000 epoch 就 ready 了，这是 spec 设计时专门留的后台优化空间。


replay 查 bug 约束（对于跑了很久的bug）
不要一上来猜原因 + 追代码，先看，相关高度触发了哪些 corner case（合约销毁？（涉及 get all 和 delete all）异常共识区块 reward？（涉及 reward 分钱）特定 PoS 事件？特定 CIP 激活节点？era / snapshot 切换点？特殊 storage 读写 pattern（涉及 mpt 核心）？罕见 OPCODE？（涉及 env 上下文参数正确与否））等，然后再有选择地追核心。
很多问题需要在代码中临时加日志查看运行时状态，才能准确回答。

工作约束
1. Monitor 不可以使用 &
2. test 必须使用 release + debug_assert, 禁止用 debug 跑 test
3. 性能瓶颈分析必须有证据，而不是凭猜测这里发生了资源争用、那里xxx逻辑引入了新开销
4. 长任务放后台 + async 定时监控。放后台任务第一时间检查正常运行。定时监控不允许跑长任务 / 盲等。


  1. MPT 和标准实现有大量修改，参考 trie.rs

AI 扫出来的注意事项

  2. deposit/vote 的解码在 delta 和 canonical 之间不对称
  - delta(from_delta_mpt_key:220/222):rest.starts_with(DEPOSIT_LIST_PREFIX) ——
  允许后缀并丢弃。
  - canonical(from_key_bytes:275/277):rest == DEPOSIT_LIST_PREFIX ——
  要求精确相等。

  5. deferred / reward 的 epoch 对齐和内存窗口(在 replay_exec.rs,不在 MPT
  里但和根校验绑死)
  - deferred_commitment_height = H − DEFERRED_STATE_EPOCH_COUNT(5)、reward 取
  REWARD_EPOCH_COUNT 前——这俩对齐错一个就全盘失配。
  - 内存 bounding 用 split_off(H − DEFERRED_STATE_EPOCH_COUNT − 1) / −
  REWARD_EPOCH_COUNT − 1 砍旧 commitment/epoch。这种 saturating_sub 的 ±1
  边界正是最容易差一位的地方——如果砍早了,后面那次 deferred 查找会拿不到 commitment
  直接 Err(硬失败,好);但如果 reward 窗口砍早了,reward
  计算会少算/报错。值得专门跑个跨这些边界的小测试确认。

  6. genesis 在 delta、首次轮转对齐
  minimal_backend 注释说 genesis 不提交、留在 delta(height 0),advance_after_commit
  跳过 height 0,让首次 snapshot 轮转落在和真实 backend 同一个 epoch。这是个真
  corner,目前靠注释保证,建议有针对性测试。