# Conflux Merkle Patricia Trie 规格

本文档定义 Conflux MPT 的计算规则。面向从零实现的开发者，阅读前提：了解 keccak-256 哈希函数和键值存储的基本概念。

---

## 1. 总体结构

Conflux 的世界状态由三个有序层组成：

| 层 | 键编码 | 可变性 | 内容 |
|---|---|---|---|
| **Snapshot** | canonical | 只读 | 上一次旋转边界时的全量状态 |
| **Intermediate** | delta | 只读 | 上一个 period 的增量 |
| **Delta** | delta | 可读写 | 当前 period 的增量 |

每一层维护一组键值对。Delta 和 intermediate 的记录分两种：**活跃值**（键存在，值为给定字节）和 **tombstone**（键已被显式删除）。Snapshot 只包含活跃值。

读取一个键时，按 delta → intermediate → snapshot 的顺序查找，返回第一个命中的结果。命中 tombstone 视为该键已删除，不再继续向下层查找——上层的删除由此遮蔽下层的值。如果三层都未命中，则键不存在。

每一层的键值对构成一棵 16 叉 trie，各自独立计算出一个 32 字节的 Merkle 根。三个根拼接后取 keccak 得到 state root：

    state_root = keccak(snapshot_root ‖ intermediate_root ‖ delta_root)

三个根中任何一个计算错误，state root 都会偏离。

下面先定义单棵 trie 的 Merkle 根如何计算（§2），再定义各层使用的键编码（§3）和其他机制（§4–§7）。

---

## 2. Merkle 哈希

### 2.1 键展开为 nibble 序列

键是字节序列。计算 Merkle 根之前，每个键先展开为 **nibble 序列**：每个字节拆成高 4 位和低 4 位两个 nibble，每个 nibble 取值 0–15。一个 n 字节的键展开为 2n 个 nibble。

例：键 `0xA7` 展开为 `[10, 7]`；键 `0x1F03` 展开为 `[1, 15, 0, 3]`。

### 2.2 Trie 的分叉与节点

所有键展开为 nibble 序列后，trie 按 nibble 逐层分叉：深度 0 按第 0 个 nibble 分组，深度 1 在每组内按第 1 个 nibble 再分组，依此类推。每一个分叉点就是一个**节点**。

一个节点具有以下属性：

- **16 个子槽** `child_0, child_1, …, child_15`：对应 nibble 值 0–15。分叉时 nibble 值为 i 的那组进入 `child_i`。
- **可选的值**：如果某个键的 nibble 序列恰好在当前深度终止（没有更多 nibble），该键的值归属于这个节点。

实现上不需要构建显式的 trie 数据结构。可以将键值对按键排序，递归地按当前深度的 nibble 分组，直接计算哈希。

### 2.3 节点哈希

**核心关系：父节点的 `child_i` 存储的是第 i 个子节点的最终哈希。** 整棵 trie 的 Merkle 根就是根节点的最终哈希。计算自底向上递归进行。

一个节点的最终哈希分两步。

**第一步——核心哈希：**

    core_hash = keccak('n' ‖ child_0 ‖ child_1 ‖ … ‖ child_15 [‖ 'v' ‖ value])

规则：

- 字节 `'n'`（0x6E）始终写入。
- 16 个子槽依次写入，每个 32 字节。空槽填 `MERKLE_NULL_NODE`（§2.6）。即使节点没有任何子节点（叶节点），也写满 16 个空槽。不使用 RLP，不做紧凑编码。
- 值后缀有三种情况，产生三种不同的哈希：

  | 值状态 | 写入内容 | 含义 |
  |---|---|---|
  | 活跃值 | `'v' ‖ value_bytes` | 一条记录在此深度终止，值为 `value_bytes` |
  | Tombstone | 仅 `'v'`，无后续内容 | 一条记录在此深度终止，标记为已删除 |
  | 无值 | 不写入 `'v'` | 此深度没有键终止，节点只是分叉点 |

  Tombstone 和无值的区别至关重要：tombstone 写入了 `'v'` 字节（后面紧跟的内容长度为零），无值则完全没有 `'v'`。如果将 tombstone 当作无值处理（省略 `'v'`），该键的删除标记就从哈希中消失了——读取时会穿透到下层，使已删除的值重新出现。

**第二步——压缩路径包裹：**

如果节点有压缩路径（§2.4），将路径编码和核心哈希拼接后再取 keccak：

    final_hash = keccak(path_info ‖ compressed_nibbles ‖ core_hash)

如果没有压缩路径，最终哈希直接等于核心哈希。

**父节点 `child_i` 存储的是子节点的 `final_hash`——压缩路径（如有）已经包含在内。**

### 2.4 路径压缩

当一段连续深度上所有键的 nibble 完全相同（没有分叉），不需要为每个深度建立单独的节点。这段无分叉的公共 nibble 前缀称为**压缩路径**，编入它下方那个节点的最终哈希（§2.3 第二步）。

以太坊 MPT 用独立的 extension 节点承载公共前缀。Conflux 没有 extension 节点——压缩路径直接编入目标节点的最终哈希，与核心哈希一起参与 keccak。

#### 编码规则

    final_hash = keccak(path_info ‖ compressed_nibbles ‖ core_hash)

三个组成部分：

**`path_info`**（1 字节）：

    path_info = 128 + 64 × skip_first + (nibble_count % 63)

- `nibble_count`：压缩路径的 nibble 数量。
- `skip_first`：若压缩路径的起始深度为奇数则为 1，否则为 0。即 `skip_first = start_depth % 2`。
- nibble 数量按模 **63** 编码，不是模 64。使用模 64 会在 nibble_count ≥ 63 时产生不同的 path_info 值——这种情况在深层 storage key 上会发生。

**`compressed_nibbles`**（字节序列）：将 nibble 序列打包为字节。

- 若 `skip_first = 1`，第一个 nibble 单独占一个字节（值为该 nibble，0x00–0x0F）。
- 其余 nibble 两两配对：前一个放高 4 位，后一个放低 4 位。若最后只剩一个 nibble，低 4 位补 0。

**`core_hash`**（32 字节）：第一步算出的核心哈希。

**`skip_first` 取决于起始深度的奇偶，而非 nibble 数量的奇偶。** 两者可以不同：例如起始深度 2（偶数）、nibble_count 3（奇数），此时 `skip_first = 0`。用 nibble_count 的奇偶决定 skip_first 会导致字节对齐错位，产生不同的哈希。

### 2.5 根节点

根节点（深度 0）不做路径压缩。即使所有键共享公共前缀，根节点的最终哈希直接等于核心哈希，不执行 §2.3 第二步。路径压缩只适用于深度 > 0 的节点。

### 2.6 空 Trie 与 MERKLE_NULL_NODE

    MERKLE_NULL_NODE = keccak(空字节串) = 0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470

当一层没有任何键值对时，该层的根为 `MERKLE_NULL_NODE`。节点中空的子槽也填此值。

完全空状态的 state root 为 `keccak(MERKLE_NULL_NODE ‖ MERKLE_NULL_NODE ‖ MERKLE_NULL_NODE)`——不等于 `MERKLE_NULL_NODE` 本身。

---

## 3. 键编码

三层使用相同的 Merkle 哈希算法（§2），但使用不同的键编码。Snapshot 使用 canonical 键，delta 和 intermediate 使用 delta 键。两种编码产生不同的 nibble 序列，同一组逻辑键在不同层中构建出不同形状的 trie。

### 3.1 Canonical 键（Snapshot 层）

Canonical 键由地址和类型后缀直接拼接，没有哈希或随机化：

| 类型 | 编码 |
|---|---|
| Account | `address`（20 字节） |
| StorageRoot | `address ‖ "data"` |
| Storage | `address ‖ "data" ‖ storage_key` |
| CodeRoot | `address ‖ "code"` |
| Code | `address ‖ "code" ‖ code_hash` |
| DepositList | `address ‖ "deposit"` |
| VoteList | `address ‖ "vote"` |

eSpace（以太坊兼容空间）的键在地址之后（第 20 字节位置）插入标记字节 `0x81`。

### 3.2 Delta 键（Delta / Intermediate 层）

Delta 键对地址部分做哈希随机化，使相邻地址在 trie 中分散到不同位置。具体方式是将地址的前 12 字节替换为哈希派生的前缀，使 32 字节的 account_key 成为所有 delta 键的公共前缀：

    account_key = keccak(padding[0..12] ‖ address)[0..12] ‖ address

其中 `padding` 是一个 32 字节的值，由当前 snapshot root 和 intermediate root 派生：

    padding = keccak(snapshot_root ‖ intermediate_root)

padding 每个 period 都会变化（§4），因此同一地址在不同 period 中映射到 trie 的不同位置。

account_key 之后追加类型后缀，规则与 canonical 键相同（`"data"`、`"code"` 等）。但 Storage 类型有额外的随机化——在 `"data"` 前缀与 storage_key 之间插入 28 字节的哈希片段：

    storage_key_hash = keccak(padding ‖ storage_key)
    storage_delta_key = account_key ‖ "data" ‖ storage_key_hash[4..32] ‖ storage_key

跳过 `storage_key_hash` 的前 4 字节（与 `"data"` 前缀等长），使得 `"data"`(4 字节) + hash[4..32](28 字节) 合计正好 32 字节。解码时跳过类型前缀后的前 32 字节即可恢复 storage_key。其他类型（CodeRoot、Code、DepositList、VoteList、StorageRoot）在 account_key 之后直接追加类型前缀和后缀，没有额外的随机化。

**eSpace 标记位置的差异：** 在 delta 键中，`0x81` 标记字节位于第 32 字节（account_key 之后），而非 canonical 键的第 20 字节（address 之后）。混淆这两个位置是常见的实现错误。

**解码的脆弱假设：** 解码器通过检查第 32 字节的最高位（`& 0x80`）来区分 eSpace 标记和类型前缀。这依赖于所有类型前缀（`"data"`、`"code"`、`"deposit"`、`"vote"`）的首字节高位为 0（小写 ASCII）。引入首字节高位为 1 的新类型前缀会导致解码静默出错。

### 3.3 短地址键

长度不足 20 字节的键在 delta 编码中原样保留，不做 padding 和哈希。将短键送入 padding 哈希会产生错误的 account_key。

### 3.4 AddressPrefix

`AddressPrefix` 不是可存储的键类型，只用于前缀读取和前缀删除。它只包含地址的一部分，无法计算完整的 delta padding，因此在 delta 键空间中编码为**空向量**。这对前缀操作有重要影响（§5）。

### 3.5 Deposit/Vote 键的编解码不对称

Delta 解码器（`from_delta_mpt_key`）对 DepositList 和 VoteList 使用前缀匹配（`starts_with`），静默丢弃前缀后的尾随字节。Canonical 解码器（`from_key_bytes`）使用精确匹配（`==`）。目前这些键没有后缀，两者行为一致。如果未来添加后缀数据，delta→canonical 转换会静默丢弃，导致合并时数据损坏。

---

## 4. 层旋转（Period 边界）

每隔 `snapshot_epoch_count` 个 epoch（默认 2000），三层进行一次旋转。旋转在 commit 内部、delta root 计算完成之后执行。返回给调用者的 CommitRoot 反映旋转前的根；旋转为下一个 period 做准备。

旋转包含四步：

1. **合并（Merge）**：将 intermediate 的每条记录从 delta 键解码为逻辑键，再编码为 canonical 键，应用到 snapshot。活跃值插入 snapshot；tombstone 从 snapshot 中删除对应键。
2. **提升（Promote）**：当前 delta 成为新的 intermediate。
3. **重置（Reset）**：delta 清空。
4. **重算 padding**：新的 delta padding 由合并后的新 snapshot root 和新 intermediate root 派生。

        new_padding = keccak(new_snapshot_root ‖ new_intermediate_root)

    新 intermediate root 等于旋转前的 delta root（因为旧 delta 被提升为新 intermediate）。

**旋转时机不可延迟。** 如果 period 边界到了却没有触发旋转，intermediate 和 snapshot 冻结在旧值上，后续所有 state root 偏离。

**padding 必须用新的 root 计算。** 使用旧 root（合并前的 snapshot root 或提升前的 intermediate root）会导致后续所有 delta 键编码偏离。

**account_key 缓存与 padding 旋转：** 如果实现缓存了 `account_key` 的计算结果，缓存必须在 padding 变化时失效。旧 delta 的缓存可以直接用作新 intermediate 的缓存（新 intermediate 的 padding 等于旧 delta 的 padding），但新 delta 的缓存必须从空开始。

---

## 5. 前缀操作

### 5.1 前缀编码的限制

前缀操作（按前缀读取、按前缀删除）在 snapshot 层上直接使用字节前缀匹配——canonical 键没有哈希处理，前缀匹配符合预期。

Delta 和 intermediate 层有问题。Delta 键的编码涉及对完整字段的哈希（address 被整体哈希以派生 account_key 的前 12 字节，storage_key 被整体哈希以派生 storage_key_hash）。一个字节前缀能否正确映射为 delta 空间中的字节前缀，取决于前缀的边界是否落在某个必须整体参与哈希的字段内部。

三种情况：

| 前缀类型 | 被哈希字段是否完整 | Delta 编码结果 |
|---|---|---|
| 地址完整，storage_key 完整或不涉及 | 是 | 正确的 delta 前缀 |
| 地址不完整（AddressPrefix） | 否 | 空向量——匹配全部 delta 条目 |
| 地址完整但 storage_key 不完整 | 否 | 错误的 hash——匹配不到目标 |

### 5.2 前缀读取

`get_all_by_prefix` 按 delta → intermediate → snapshot 顺序扫描，收集每个 canonical 键的首次出现。tombstone 完全抑制该键，不包含在结果中。

对 AddressPrefix，因其 delta 前缀为空，会遍历全部 delta/intermediate 条目，再按 canonical 地址过滤。

### 5.3 前缀删除

`delete_all_by_prefix` 处理三层：

1. **Delta**：匹配的条目直接移除（当前 period 的写入，移除即可）。
2. **Intermediate**：不修改 intermediate 本身（逻辑只读）。对每个可见的 intermediate 条目，通过 `set` 在 delta 中插入 tombstone。
3. **Snapshot**：对每个匹配的 snapshot 条目，通过 `set` 在 delta 中插入 tombstone。

两个执行顺序要求：

- **先清除 delta 再插入 tombstone。** 如果先插入 tombstone 再清除 delta，或者没有先清除 delta，残留的旧 delta 条目会遮蔽后续新写入。
- **必须为 intermediate 和 snapshot 的匹配条目都插入 tombstone。** 如果只移除了 delta 条目却不为下层插入 tombstone，被删除的值在下次读取时会穿透到 intermediate 或 snapshot，重新出现。

### 5.4 已知限制

**AddressPrefix 删除的副作用：** AddressPrefix 的 delta 前缀为空，前缀扫描命中全部 delta 条目。实现先将命中的条目全部从 delta 移除，再按 canonical 地址过滤返回值。因此 `delete_all_by_prefix(AddressPrefix(x))` 会移除 delta 中的所有条目（不限于地址匹配的），尽管返回值只包含地址匹配的那些。这是与上游一致的已知行为。

**部分 storage_key 前缀无法删除 delta/intermediate 条目。** 不完整的 storage_key 计算出错误的 storage_key_hash，在 delta 中匹配不到目标条目。只有 snapshot 层的条目能被正确删除。这是 delta 编码方案的固有限制，无法修复。典型场景：合约自毁时清空 sponsor whitelist，白名单条目的 storage_key 以被杀合约地址为前缀，但该前缀不构成完整的 storage_key。需要等条目经过两次旋转迁入 snapshot 后才能正确删除。

---

## 6. 值语义

### 6.1 三种状态

Delta 和 intermediate 中的每条记录处于三种状态之一。三种状态在读取语义和哈希输出上都不同（哈希差异见 §2.3）：

- **活跃值** `Some(bytes)`：键存在，值为 `bytes`。读取时返回该值。
- **Tombstone**：键已被显式删除。读取时返回"不存在"，不向下层穿透。如果从 delta 中移除 tombstone（而非保留），下层的同名键将在读取时重新可见。
- **无记录**：该键在此层没有条目。读取时向下层穿透。

Snapshot 只包含活跃值。合并时，intermediate 的 tombstone 转化为对 snapshot 对应键的删除。

### 6.2 设空即删除

将一个键的值设为空字节切片等同于删除：实现存储 tombstone，而非零长度的活跃值。调用者不需要区分"删除"和"设空"——两者是同一操作。

---

## 7. 持久化

持久化的状态包含：

| 内容 | 说明 |
|---|---|
| Snapshot | canonical 键 → 活跃值（tombstone 不进入 snapshot） |
| Intermediate / Delta | delta 键 → 活跃值或 tombstone |
| Padding | intermediate 和 delta 各自的 32 字节 padding |
| Height | epoch 计数，用于判断 period 边界 |
| Snapshot epoch count | 旋转间隔 |
| Last root | 最近一次 commit 的 CommitRoot |

Delta 和 intermediate 的 tombstone 必须持久化——它们承载读取时的删除语义，丢失 tombstone 会导致已删除的值重新出现。
