# Conflux 执行层数据包格式规范

## 1 概述

本规范定义了一种二进制格式，用于紧凑编码 Conflux 执行层数据。每个数据包覆盖连续 2000 个 epoch 的完整执行数据，可由相邻数据包串联实现全链历史重放。

格式在空间效率与碰撞概率之间做了显式取舍：部分哈希字段仅存储前缀（4 字节），签名和可重算的验证字段被省略。

## 2 基础编码类型

### 2.1 LEB128 变长整数

ULEB128（无符号）和 SLEB128（有符号）采用标准 LEB128 编码，每字节低 7 位为有效数据，最高位为延续标记。

### 2.2 QC8

定长编码，用于大整数（如 reward 值）。首字节高 2 位指示总长度：

| 高 2 位 | 总字节数 | 有效位数 |
|---------|---------|---------|
| `00`    | 8       | 62      |
| `01`    | 9       | 70      |
| `10`    | 10      | 78      |
| `11`    | 11      | 86      |

首字节剩余 6 位与后续字节共同组成无符号整数的大端表示。

### 2.3 QC5E

定长编码，支持枚举特殊值。首字节高 2 位指示模式：

| 高 2 位 | 模式 | 总字节数 | 说明 |
|---------|-----|---------|-----|
| `00`    | 枚举 | 1       | 低 6 位为枚举值；枚举 0 恒定代表零 |
| `01`    | 整数 | 5       | 38 有效位 |
| `10`    | 整数 | 6       | 46 有效位 |
| `11`    | 整数 | 7       | 54 有效位 |

整数模式下，首字节剩余 6 位与后续字节共同组成无符号整数的大端表示。各字段的枚举值含义由该字段自行定义（见 §6.3）。

## 3 数据包总体布局

一个数据包由以下段按序排列组成：

```
┌──────────────────────────┐  offset 0
│        Header            │
├──────────────────────────┤
│    地址查找表             │
├──────────────────────────┤
│    PoS 查找表             │
├──────────────────────────┤
│    Difficulty 查找表      │
├──────────────────────────┤
│    Sender Base Nonce 表   │
├──────────────────────────┤
│    Gas Price 查找表       │
├──────────────────────────┤
│    区块段头               │  ← 对齐到 Header 起点 + 32 字节倍数
├──────────────────────────┤
│    区块段正文             │
├──────────────────────────┤
│    交易段                 │  ← 对齐到 Header 起点 + 64 字节倍数
└──────────────────────────┘
```

各段的起始位置由 Header 中的偏移表指定。

## 4 Header

Header 位于数据包起始位置（offset 0），由固定字段区和偏移表组成。

### 4.1 固定字段

| 偏移 | 字段 | 大小 | 说明 |
|------|------|------|------|
| 0    | `prev_last_hash` | 32 字节 | 上一个数据包最后一个区块的 hash |
| 32   | `prev_last_deferred_state_root` | 32 字节 | 上一个数据包最后一个区块的 deferred_state_root |
| 64   | `first_block_number` | 8 字节 | 当前数据包第一个区块的 block number |
| 72   | `min_timestamp` | 8 字节 | 当前数据包区块头 timestamp 的最小值 |
| 80   | `min_height` | 8 字节 | 范围内区块头 height 的最小值 |
| 88   | `min_pos_height` | 4 字节 | 范围内 PoS 区块高度的最小值 |
| 92   | `block_prefix_size` | 1 字节 | 区块段正文中每个区块定长前缀的字节数（记为 N） |

固定字段区总长 93 字节。多字节整数采用小端序。

**`block_prefix_size` 选定规则**：N 的取值为 {64, 72, 80, 88, 96}。编码器从 N = 64 开始，逐级判断是否膨胀：

1. 统计本数据包中编码长度不超过 N 字节的区块占全部区块的比例 P。
2. 若 N < 80 且 P < 90%，则 N += 8，重复步骤 1。
3. 若 N ≥ 80 且 P < 70%，则 N += 8，重复步骤 1。
4. 否则选定当前 N。N = 96 为上限，不再膨胀。

### 4.2 偏移表

紧跟固定字段区之后。每项 4 字节（小端序 u32），表示相对于 Header 起点的字节偏移。

| 序号 | 偏移目标 |
|------|---------|
| 0    | 地址查找表 |
| 1    | PoS 查找表 |
| 2    | Difficulty 查找表 |
| 3    | Sender Base Nonce 表 |
| 4    | Gas Price 查找表 |
| 5    | 区块段头 |
| 6    | 区块段正文 |
| 7    | 交易段 |

偏移表总长 32 字节（8 × 4）。

## 5 查找表

查找表将高频出现的值映射为紧凑的整数索引。区块编码和交易编码通过 ULEB128 索引引用查找表条目。

### 5.1 地址查找表

条目格式：H160（20 字节），按出现频率降序排列。索引 0 对应最高频地址。

### 5.2 PoS 查找表

条目格式：

| 字段 | 大小 | 说明 |
|------|------|------|
| `pos_block_hash` | 32 字节 | PoS 区块头哈希（H256） |
| `pos_height_offset` | 2 字节 | 相对于 `min_pos_height` 的 PoS 区块高度偏移 |

### 5.3 Difficulty 查找表

条目格式：U256（32 字节）。

### 5.4 Sender Base Nonce 表

条目格式：

| 字段 | 编码 | 说明 |
|------|------|------|
| `sender_index` | ULEB128 | 地址查找表中的索引 |
| `base_nonce` | ULEB128 | 该 sender 在本数据包中的 nonce 基准值 |

仅收录空间收益超过 16 字节的 sender（即该 sender 所有交易中 `Σ(裸 nonce ULEB128 长度 − nonce offset ULEB128 长度) ≥ 16`）。交易中的 nonce 将编码为相对于 `base_nonce` 的偏移。

### 5.5 Gas Price 查找表

条目格式：U256（32 字节）。

收录条件：在本数据包范围内出现 3 次以上，且不同值总计不超过 16 个，取出现频率最高的值。该表同时用于区块 `base_price` 和交易 `gas_price` / `max_fee_per_gas` / `max_priority_fee_per_gas` 字段的索引引用。

## 6 区块段

### 6.1 区块段头

| 字段 | 大小 | 说明 |
|------|------|------|
| `block_count` | 4 字节 | 本数据包包含的区块总数 |
| `extension_bitmap` | ⌈block_count / 8⌉ 字节 | 每 bit 对应一个区块，置 1 表示该区块的编码超出 N 字节定长前缀 |
| （padding） | 变长 | 填充 0 对齐至 Header 起点的 32 字节倍数 |

### 6.2 区块段正文

区块段正文由两部分组成：

**定长前缀区**：连续存放每个区块编码的前 N 字节，共 `block_count × N` 字节。

**变长溢出区**：仅包含 `extension_bitmap` 中标记为 1 的区块，按区块顺序排列。每个溢出条目格式为：

| 字段 | 编码 | 说明 |
|------|------|------|
| `overflow_length` | ULEB128 | 溢出数据长度 |
| `overflow_data` | 原始字节 | 区块编码中第 N 字节之后的部分 |

### 6.3 区块编码格式

每个区块按以下顺序编码为连续字节流。前 N 字节存入定长前缀区，超出部分存入变长溢出区。

| 字段 | 编码 | 说明 |
|------|------|------|
| `self_hash` | 32 字节 | 区块哈希（完整 H256） |
| `deferred_state_root` | 4 字节前缀 | 仅存储前 4 字节 |
| `deferred_receipts_root` | 4 字节前缀 | 仅存储前 4 字节 |
| `deferred_logs_bloom_hash` | 4 字节前缀 | 仅存储前 4 字节 |
| `flags` | 1 字节 | 标志位（见下表） |
| `author` | ULEB128 | 地址查找表索引 |
| `timestamp` | ULEB128 | 相对于 `min_height` 的偏移 |
| `difficulty` | ULEB128 | Difficulty 查找表索引 |
| `gas_limit` | QC5E | 枚举 1 = 30,000,000；枚举 2 = 60,000,000 |
| `base_price_core` | QC5E | Core Space 基础费；0 代表未激活；枚举 1 = 1,000,000,000（1 GDrip） |
| `base_price_espace` | QC5E | eSpace 基础费；0 代表未激活；枚举 1 = 20,000,000,000（20 GDrip）|
| `height` | ULEB128 | 相对于 `min_height` 的偏移 |
| `blame` | ULEB128 | blame 值 |
| `finalized_epoch` | ULEB128 | 相对于当前 epoch 编号的偏移（一定小于当前 Epoch 号） |
| `tx_segment_offset` | ULEB128 | 交易段内偏移（实际字节偏移 ÷ 4） |
| `base_reward` | QC8 | 区块基础奖励（`BlockRewardResult.total_reward`） |

**flags 字段位定义**：

| 位 | 名称 | 说明 |
|----|------|------|
| 0  | `adaptive` | 自适应权重标志 |
| 1  | `pivot` | 是否为 pivot 区块 |
| 2  | `espace` | 是否为 eSpace 区块（CIP-90：pivot 且 height 为 5 的倍数） |
| 3  | `has_transactions` | 是否在交易段中有对应数据块 |
| 4  | `tx_compressed` | 交易数据块是否经过压缩 |
| 5  | `skipped_execution` | 是否跳过执行（一个 epoch 中区块太多会被跳过） |
| 6  | `zero_total_reward` | 该区块的 `total_reward`（完整结算奖励，区别于 `base_reward`）为 0。corner case 标记 |
| 7  | （reserved） | 保留，置 0 |

> `zero_total_reward`（位 6）取自源 DB 的 `BlockRewardResult`（RLP 布局 `[total_reward, base_reward, tx_fee]`，取 `total_reward`）。提取时该值为 0 即置位；对已生成的归档可用 `add-total-reward-flag` 子命令原地补打（仅翻转该 bit，不改任何长度/偏移）。

当 `has_transactions = 0` 时，`tx_segment_offset` 和 `tx_compressed` 无意义。

## 7 交易段

交易段整体起始位置对齐至 Header 起点的 64 字节倍数。

### 7.1 交易数据块结构

仅 `has_transactions = 1` 的区块在交易段中有对应的数据块。每个数据块格式为：

| 字段 | 编码 | 说明 |
|------|------|------|
| `length` | ULEB128 | 数据块长度（压缩后长度，若已压缩） |
| `payload` | 原始字节 | 数据块内容 |

数据块的位置由区块编码中的 `tx_segment_offset × 64` 确定（相对于交易段起始位置）。`tx_compressed` 标志指示 `payload` 是否经过压缩；压缩仅在 RLP 编码后长度超过 512 字节时启用。

### 7.2 数据块内容

解压（或直接读取）后的数据块是一个 RLP 列表 `RLP([e₀, e₁, e₂, …])`，其中每个 item 为以下类型之一：

- **交易**：支持 CIP-155、CIP-2930 等 Core Space 类型及 EIP-155、EIP-2930 等 eSpace 类型。编码规则见 §7.3。注意：复用 RLP 算法，但不需要遵循链上标准编码如（EIP-2718）。
- **重复打包标记**：当同一笔交易在多个区块中出现时，仅首次出现完整编码，后续出现记录为 `[block_index, tx_index]` 引用。
- **PoS 事件**：包括 `PosRewardInfo` 和 `UnlockEvent`，追加在所属 epoch 最后一个区块的 RLP 列表末尾。

### 7.3 交易编码规则

交易字段按以下规则编码：

- 包含除签名（`v`、`r`、`s`）以外的所有字段；EIP-7702 authorization list 中的签名完整保留。
- **不包含** `chain_id`。
- `sender` 和 `action`（目标地址）：替换为地址查找表的 ULEB128 索引。
- `nonce`：若该 sender 在 Sender Base Nonce 表中有条目，则编码为相对于 `base_nonce` 的 ULEB128 偏移；否则编码原始值。
- `gas_price` / `max_fee_per_gas` / `max_priority_fee_per_gas`：编码为枚举类型，可以是内联原始值或 Gas Price 查找表索引。
- `epoch_height`（target epoch）：不记录绝对值，仅记录相对于交易所在区块 epoch 的绝对差值是否在阈值内。

## 8 数据提炼

本章描述如何从节点数据库快照与运行时配置中提取数据包所需的各字段值。

### 8.1 数据源概述

提炼过程依赖两个 RocksDB 数据库实例和一组运行时配置参数。

#### 8.1.1 PoW 主数据库

节点数据目录下的主数据库（`blockchain_db/`），涉及以下 column family：

| CF ID | 名称 | 用途 |
|-------|------|------|
| 1 | blocks | 区块头、区块体、执行上下文、奖励等 |
| 3 | epoch_numbers | epoch 编号 → 区块哈希列表 |
| 7 | reward_by_pos_epoch | PoS epoch 编号 → PosRewardInfo |

**blocks CF** 以区块哈希（32 字节）为 key 基础，通过追加 1 字节后缀区分不同数据类型：

| 后缀 | 值类型 | 编码 |
|------|--------|------|
| （无） | BlockHeader | RLP |
| 0x02 | 区块体（`Vec<SignedTransaction>`） | RLP |
| 0x04 | EpochExecutionContext（含 `start_block_number: u64`） | RLP |
| 0x08 | `(epoch_hash: H256, BlockRewardResult)` | RLP |

BlockRewardResult 包含三个 U256 字段：`total_reward`、`base_reward`、`tx_fee`。

**epoch_numbers CF** 以 9 字节为 key：前 8 字节为小端 u64 epoch 编号，第 9 字节为后缀：

| 后缀 | 含义 |
|------|------|
| 0x06 | 已执行区块哈希列表（RLP `Vec<H256>`）；**最后一个元素为 pivot 区块** |
| 0x07 | 被跳过执行的区块哈希列表（RLP `Vec<H256>`） |

**reward_by_pos_epoch CF** 以 8 字节大端 u64 PoS epoch 编号为 key，value 为 RLP 编码的 PosRewardInfo。

#### 8.1.2 PoS 账本数据库

位于 `{数据目录}/pos-ledger-db/`，独立的 RocksDB 实例。涉及以下 column family：

| CF 名称 | Key 格式 | Value | 编码 |
|---------|---------|-------|------|
| `committed_block` | PoS 区块哈希（32 字节） | CommittedBlock | BCS |
| `committed_block_by_view` | view（u64） | PoS 区块哈希 | BCS |
| `event` | `(version: u64, index: u64)` | ContractEvent | BCS |

CommittedBlock 包含以下字段：

| 字段 | 类型 | 说明 |
|------|------|------|
| `hash` | H256 | PoS 区块哈希 |
| `epoch` | u64 | PoS epoch（验证者集合轮次） |
| `round` | u64 | PoS 共识轮次 |
| `pivot_decision` | H256 | 该 PoS 区块决定的 PoW pivot 区块哈希 |
| `version` | u64 | 账本版本 |
| `view` | u64 | PoS 区块高度（单调递增） |
| `timestamp` | u64 | 时间戳 |

#### 8.1.3 运行时配置依赖

以下参数影响字段提炼逻辑，需从节点配置或硬编码常量中获取：

| 参数 | 默认值 | 用途 |
|------|--------|------|
| `evm_transaction_block_ratio` | 5 | espace 标志判断：pivot 且 `height % ratio == 0` |
| `pos_pivot_decision_defer_epoch_count` | 50（CIP-113 后 20） | `finalized_epoch` 推导中的延迟量 |
| `REWARD_EPOCH_COUNT` | 12 | 区块奖励在 epoch N+12 中被计算 |
| `EPOCH_EXECUTED_BLOCK_BOUND` | 200 | 单个 epoch 最大可执行区块数，超出部分标记为跳过 |

### 8.2 遍历流程

数据包覆盖连续 2000 个 epoch（记为 E₀ … E₁₉₉₉）。对每个 epoch Eᵢ：

1. 从 epoch_numbers CF 读取 key `[Eᵢ (LE u64) ‖ 0x06]`，得到已执行区块哈希列表 `[h₀, h₁, …, hₖ]`。列表最后一个元素 hₖ 为该 epoch 的 pivot 区块。
2. 从 epoch_numbers CF 读取 key `[Eᵢ (LE u64) ‖ 0x07]`（若存在），得到被跳过执行的区块哈希列表。
3. 对每个区块哈希 h，从 blocks CF 读取区块头（key = `h`）和区块体（key = `h ‖ 0x02`）。
4. 数据包中的区块按 epoch 顺序排列，每个 epoch 内按已执行列表顺序排列；被跳过的区块排在已执行区块之后。

### 8.3 Header 字段提炼

| 字段 | 提炼方式 |
|------|---------|
| `prev_last_hash` | 上一个数据包范围内最后一个 epoch 的 pivot 区块哈希（即 E₋₁ 的 0x06 列表末尾元素） |
| `prev_last_deferred_state_root` | 该 pivot 区块头的 `deferred_state_root` 字段（完整 H256） |
| `first_block_number` | E₀ 的 pivot 区块对应的 EpochExecutionContext（blocks CF，key = `pivot_hash ‖ 0x04`）中的 `start_block_number` |
| `min_timestamp` | 遍历数据包内所有区块头，取 `timestamp` 字段的最小值 |
| `min_height` | 遍历数据包内所有区块头，取 `height` 字段的最小值 |
| `min_pos_height` | 对每个区块头的 `pos_reference`（若非 None），查 pos-ledger-db `committed_block` CF 得到 `view` 字段；取所有 `view` 的最小值 |
| `block_prefix_size` | 编码时根据 §4.1 选定规则决定 |

### 8.4 查找表构建

**地址查找表**：收集所有区块头的 `author` 字段和所有交易的 `sender`、`action`（Call 目标地址）。按出现频率降序排列。`sender` 直接从 SignedTransaction 的 `sender` 字段读取（区块体 RLP 中已包含，无需从签名恢复）。

**PoS 查找表**：收集所有区块头中不重复的 `pos_reference` 值（Option\<H256\>，跳过 None）。对每个 `pos_reference`，查 pos-ledger-db `committed_block` CF：
- `pos_block_hash` = CommittedBlock 的 `hash`（即 `pos_reference` 本身）
- `pos_height_offset` = CommittedBlock 的 `view` − `min_pos_height`

**Difficulty 查找表**：收集所有区块头 `difficulty` 的不重复值。

**Sender Base Nonce 表**：统计每个 sender 在数据包范围内所有交易的 nonce 值，选定 `base_nonce`，并按 §5.4 的收益阈值筛选。

**Gas Price 查找表**：统计区块头 `base_price`（`core_base_price` 和 `espace_base_price`，CIP-1559 激活前 `base_price` 为 None）和交易的 `gas_price` / `max_fee_per_gas` / `max_priority_fee_per_gas` 字段中出现 3 次以上的不同值，取频率最高的不超过 16 个。

### 8.5 区块字段提炼

以下"区块头"指从 blocks CF 以 block_hash 为 key 读取并 RLP 解码得到的 BlockHeader。BlockHeader 的 RLP 编码结构为 `BlockHeaderRlpPart` 的字段序列，包含：`parent_hash`、`height`、`timestamp`、`author`、`transactions_root`、`deferred_state_root`、`deferred_receipts_root`、`deferred_logs_bloom_hash`、`blame`、`difficulty`、`adaptive`、`gas_limit`、`referee_hashes`、`custom`、`nonce`、`pos_reference`、`base_price`。

| 字段 | 提炼方式 |
|------|---------|
| `self_hash` | 区块哈希本身（即 blocks CF 的 key） |
| `deferred_state_root` | 区块头 `deferred_state_root` 的前 4 字节 |
| `deferred_receipts_root` | 区块头 `deferred_receipts_root` 的前 4 字节 |
| `deferred_logs_bloom_hash` | 区块头 `deferred_logs_bloom_hash` 的前 4 字节 |
| `flags.adaptive` | 区块头 `adaptive` 字段 |
| `flags.pivot` | 该区块哈希是否等于其所在 epoch 0x06 列表的最后一个元素 |
| `flags.espace` | `pivot == true` 且 `height % evm_transaction_block_ratio == 0` |
| `flags.has_transactions` | 区块体（0x02）中交易列表非空，或该区块为所属 epoch 末尾区块且该 epoch 有待追加的 PoS 事件 |
| `flags.tx_compressed` | 编码时决定：交易数据块 RLP 编码后超过 512 字节则压缩 |
| `flags.skipped_execution` | 该区块哈希出现在其所在 epoch 的 0x07 列表中 |
| `author` | 区块头 `author` → 地址查找表索引 |
| `timestamp` | 区块头 `timestamp` − `min_timestamp` |
| `difficulty` | 区块头 `difficulty` → Difficulty 查找表索引 |
| `gas_limit` | 区块头 `gas_limit`（QC5E 编码） |
| `base_price_core` | 区块头 `base_price` 若存在则取 `core_base_price`，否则为 0 |
| `base_price_espace` | 区块头 `base_price` 若存在则取 `espace_base_price`，否则为 0 |
| `height` | 区块头 `height` − `min_height` |
| `blame` | 区块头 `blame` |
| `finalized_epoch` | 见 §8.6 |
| `tx_segment_offset` | 编码时计算（该区块交易数据块在交易段中的位置 ÷ 64） |
| `base_reward` | blocks CF key = `block_hash ‖ 0x08` → RLP 解码得到 `(epoch_hash, BlockRewardResult)` → 取 `base_reward` 字段 |

### 8.6 `finalized_epoch` 推导

该字段记录 PoS 终局确认的 epoch 编号相对于当前 epoch 的偏移。推导步骤：

1. 从区块头读取 `pos_reference: Option<H256>`。若为 None（PoS 未启用），`finalized_epoch` 编码为 0。
2. 以 `pos_reference` 为 key 查 pos-ledger-db `committed_block` CF，得到 CommittedBlock。
3. 取 CommittedBlock 的 `pivot_decision`（一个 PoW 区块哈希），在 blocks CF 中读取该区块头的 `height` 字段，记为 `pd_height`。
4. 计算延迟后的终局高度：`finalized_height = pd_height − pos_pivot_decision_defer_epoch_count`。该配置值在 CIP-113 激活前为 50，激活后为 20。
5. 编码值 = 当前区块所在 epoch 编号 − `finalized_height`。

### 8.7 交易提炼

交易数据从 blocks CF key = `block_hash ‖ 0x02` 读取，RLP 解码为 `Vec<SignedTransaction>`。每个 SignedTransaction 的 RLP 结构为三元素列表：

1. `TransactionWithSignature`：含无签名交易体 + 签名（v、r、s）
2. `sender: Address`（20 字节，已预先恢复并存储）
3. `public: Option<Public>`（可选公钥）

无签名交易体按类型区分：

**Core Space**（三种活跃类型）：
- CIP-155：`nonce`、`gas_price`、`gas`、`action`、`value`、`storage_limit`、`epoch_height`、`chain_id`、`data`
- CIP-2930：同上 + `access_list`
- CIP-1559：`nonce`、`max_priority_fee_per_gas`、`max_fee_per_gas`、`gas`、`action`、`value`、`storage_limit`、`epoch_height`、`chain_id`、`data`、`access_list`

**eSpace**（四种类型）：
- EIP-155：`nonce`、`gas_price`、`gas`、`action`、`value`、`chain_id`（可选）、`data`
- EIP-2930：`chain_id`、`nonce`、`gas_price`、`gas`、`action`、`value`、`data`、`access_list`
- EIP-1559：`chain_id`、`nonce`、`max_priority_fee_per_gas`、`max_fee_per_gas`、`gas`、`action`、`value`、`data`、`access_list`
- EIP-7702：`chain_id`、`nonce`、`max_priority_fee_per_gas`、`max_fee_per_gas`、`gas`、`destination`、`value`、`data`、`access_list`、`authorization_list`

提炼规则：
- `sender` 直接从 SignedTransaction 的第二个 RLP 元素读取，替换为地址查找表索引
- `action`（Call 目标地址）替换为地址查找表索引
- 签名字段（v、r、s）丢弃；EIP-7702 `authorization_list` 中的签名完整保留
- `chain_id` 丢弃
- `nonce`：若 sender 在 Sender Base Nonce 表中有条目，编码为相对 `base_nonce` 的偏移
- `gas_price` / `max_fee_per_gas` / `max_priority_fee_per_gas`：匹配 Gas Price 查找表则编码为索引，否则内联原始值
- `epoch_height`（仅 Core Space）：编码为相对于所在区块 epoch 的偏移

**重复打包检测**：以交易哈希为 key 跟踪已编码交易。同一笔交易在多个区块中出现时，仅首次完整编码，后续出现记录为 `[block_index, tx_index]` 引用。

### 8.8 PoS 事件提炼

#### PosRewardInfo

从 reward_by_pos_epoch CF（ID = 7）按 PoS epoch 编号读取。Value 为 RLP 编码的 PosRewardInfo：

| 字段 | 类型 | 说明 |
|------|------|------|
| `account_rewards` | `Vec<{address: H160, pos_identifier: H256, reward: U256}>` | 各账户的 PoS 奖励 |
| `execution_epoch_hash` | H256 | 奖励分配所在的 PoW epoch 哈希 |

通过 `execution_epoch_hash` 将 PosRewardInfo 关联到 PoW epoch：若该哈希属于当前数据包某个 epoch 的 pivot 区块，则将该 PosRewardInfo 追加到该 epoch 最后一个区块的交易段 RLP 列表末尾。

扫描范围：需遍历可能覆盖当前数据包的 PoS epoch。具体范围由数据包内区块头的 `pos_reference` 涉及的 PoS epoch 值界定。

#### UnlockEvent

从 pos-ledger-db `event` CF 读取。Key 为 `(version: u64, index: u64)`，value 为 BCS 编码的 ContractEvent。解析后得到：

| 字段 | 类型 | 说明 |
|------|------|------|
| `node_id` | AccountAddress | 被解锁的 PoS 节点地址 |
| `unlocked` | u64 | 解锁的投票数 |

UnlockEvent 通过 PoS epoch 对应的 PoW execution epoch 确定归属，追加方式与 PosRewardInfo 相同。
