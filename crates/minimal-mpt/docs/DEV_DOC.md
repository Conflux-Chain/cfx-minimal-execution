# Conflux MPT 开发笔记

本文档描述 `minimal-mpt` 中实现的 Conflux Merkle Patricia Trie。面向实现者编写，重点说明 Conflux 设计与标准以太坊 MPT 的差异，以及历史上踩过的坑。

---

## 1. 三段式状态模型

Conflux 的世界状态不是一棵单独的 trie，而是三个有序层的叠加：

| 层 | 键空间 | 当前 period 内的可变性 | 内容 |
|---|---|---|---|
| **Snapshot** | Canonical（原始） | 只读 | 上一次 snapshot 边界时的全量活跃值 |
| **Intermediate** | Delta（带 padding） | 只读 | 上一个 period 的增量（即上一轮的 delta） |
| **Delta** | Delta（带 padding） | 可读写 | 当前 period 内累积的修改 |

读取时按 delta → intermediate → snapshot 的顺序查找，第一个命中的层即为结果。上层的 tombstone 会遮蔽下层的活跃值。

### 1.1 State Root

State root 不是任何单棵 trie 的根。它是三段根的拼接哈希：

    state_root = keccak(snapshot_root ‖ intermediate_root ‖ delta_root)

三个 32 字节的根按序拼接后取 keccak。这意味着三个根都必须正确计算，state root 才能匹配。

### 1.2 Snapshot 旋转（Period 边界）

每隔 `snapshot_epoch_count` 个 epoch（默认 2000），三层进行旋转：

1. **合并（Merge）**：将 intermediate 的每一条记录应用到 snapshot 中。活跃值按 canonical 键插入 snapshot；tombstone 则从 snapshot 中删除对应的 canonical 键。
2. **提升（Promote）**：刚结束的 delta 成为新的 intermediate。
3. **重置（Reset）**：delta 清空。
4. **重算 padding**：delta 的 key padding 发生变化（见 §2），因为它依赖于新的 snapshot root 和新的 intermediate root。

**陷阱 — commit 之后必须推进状态。** 曾经有一个 bug：`commit()` 只算了 delta root 却没有触发旋转逻辑。如果 period 边界到了却没有旋转，intermediate 和 snapshot 就会冻结，后续所有 state root 都会偏离。旋转必须在 delta root 算完*之后*（delta root 要成为新 intermediate root）且在下一个 epoch 的写入*之前*（新写入需要新 padding）进行。

---

## 2. 键空间

### 2.1 Canonical 键（Snapshot 用）

Canonical 键是"原始"编码：20 字节的地址后面跟类型前缀和后缀，没有哈希也没有随机化。

各存储键类型的编码：

| 类型 | 编码 |
|---|---|
| Account | `address`（20 字节） |
| StorageRoot | `address ‖ "data"` |
| Storage | `address ‖ "data" ‖ storage_key` |
| CodeRoot | `address ‖ "code"` |
| Code | `address ‖ "code" ‖ code_hash` |
| DepositList | `address ‖ "deposit"` |
| VoteList | `address ‖ "vote"` |

eSpace（以太坊兼容空间）的键在第 20 字节（紧接地址之后）插入标记字节 `0x81`。


### 2.2 Delta 键（Delta 和 Intermediate 用）

Delta 键对地址部分做了随机化，使得相邻地址不会在 trie 中聚集。前 12 字节被替换为哈希派生的前缀：

    account_key = keccak(padding[0..12] ‖ address)[0..12] ‖ address

其中 `padding = keccak(snapshot_root ‖ intermediate_root)`。这个 padding 每个 period 都会变，因此同一个地址在不同 period 中映射到 trie 的不同位置。

完整的 delta 键在 account_key 之后追加类型前缀，对 storage 类型还有进一步的随机化：

    storage_delta_key = account_key ‖ "data" ‖ storage_key_padding ‖ storage_key
    storage_key_padding = keccak(padding ‖ storage_key)

eSpace 的 delta 键中，`0x81` 标记字节插入在第 32 字节（32 字节的 account key 部分之后），而不是第 20 字节。这个位置差异是一个常见的混淆来源。

**陷阱 — eSpace 标记检测很脆弱。** 解码器通过检查第 32 字节的高位（`& 0x80`）来区分 eSpace 和类型前缀。这之所以能工作，是因为所有类型前缀（`"data"`、`"code"`、`"deposit"`、`"vote"`）的首字节都是小写 ASCII（高位为 0）。如果增加一个首字节高位为 1 的前缀，解码就会静默出错。

### 2.3 短地址键

长度不足 20 字节的地址在 delta 编码中原样透传，不做 padding 和哈希。这是有意为之：允许部分键查找和内部前缀标记保持不变。

### 2.4 AddressPrefix — 只读构造

`AddressPrefix`（上游类型名 `AddressPrefixKey`）不是一个真正可存储的键类型，只用于前缀读取/扫描与前缀删除，从不写入。在 delta 键空间里它编码为**空向量** —— 因为不含完整地址，无法做哈希编码；在 canonical 空间里则原样保留地址前缀字节。这个空编码对前缀删除有特殊后果（会波及无关 delta 条目），见 §5.3。

实践中它基本只用于读取/dump：共识执行删除合约 storage 时走的是 `StorageRoot` 或部分 storage 键前缀（见 §5.3），并不经过 `AddressPrefix`。

---

## 3. Merkle 哈希

每一段（snapshot / intermediate / delta）的根是一棵 16 叉 trie 的 Merkle 根。键先展开成 **nibble**（半字节）：每个字节拆成高、低两个 4-bit nibble，键于是变成一串取值 0..15 的 nibble 序列，trie 按 nibble 逐层分叉，每个节点最多 16 个分支。

回顾标准以太坊 MPT：它有**三种** RLP 编码的节点 —— leaf（键的剩余部分 + 值）、extension（一段共享路径 + 指向唯一子节点的指针）、branch（16 个子节点槽 + 第 17 个值槽）。Conflux 把这三种角色**合并进同一种节点**，一个节点可同时具备以下三部分（都可选）：

- **一段压缩路径**（取代 extension 节点）：通向本节点的那段共享 nibble 前缀，即本节点的"入边路径"；
- **最多 16 个子节点**（branch 的角色）：固定 16 个子槽；
- **一个值**（取代 leaf，以及 branch 的第 17 槽）：可与子节点并存。

所以这里**没有独立的 extension 节点**。关键问题是：以太坊把共享路径放在 extension 节点里，没有 extension 节点时这段路径放哪？答案是**挂到它所通向的那个节点上**——它成为那个节点自己的入边路径，并被编进该节点的哈希（见 §3.2）。父节点的子槽里存的，就是子节点的哈希——它的入边路径（若有）已经编了进去。

节点不用 RLP，各字段用标签字节直接拼接后做 keccak（见 §3.1、§3.2）。这棵 trie 也不持久化节点结构，root 是从一组排好序的 (key, value) 直接递归算出来的：每一层把当前 nibble 相同的键归为一组（递归成一个子节点），这组键共享的 nibble 前缀就成为该子节点的入边路径，而键长正好在本层结束的那条记录提供本节点的值。

### 3.1 节点哈希

一个节点的哈希分两步算。第一步，对节点的**子节点和值**算出核心哈希：

    node_hash = keccak('n' ‖ child_0 ‖ child_1 ‖ … ‖ child_15 [‖ 'v' ‖ value])

`'n'` 标签字节始终存在；其后是**恒定 16 个**子槽，每个 `child_i` 占 32 字节，存对应子节点的哈希（下面定义），空槽填 `MERKLE_NULL_NODE` —— 即便没有任何子节点的叶子也照写满 16 个 null 槽。值后缀只在有一条记录正好终止于本节点时才追加：`'v'` 作分隔符，其后接 value（tombstone 则这段 value 为空字节）；不终止任何记录的纯分叉节点没有这段后缀（三种值状态见 §4.1）。这里不用 RLP，也没有以太坊那种 2 子节点紧凑编码或"值放第 17 槽"的约定。

第二步，如果节点有入边路径（§3.0），就把路径包在核心哈希外得到节点哈希（算法见 §3.2）；没有入边路径就省去这步，节点哈希等于核心哈希。父节点子槽里的 `child_i`，存的就是对应子节点这样算出的节点哈希。

### 3.2 路径压缩（节点的入边路径）

第二步为何存在：若严格按 nibble 逐层建节点，一段没有分叉的公共前缀会退化成一长串"只有单个子节点"的节点。以太坊用 extension 节点解决，Conflux 没有 extension 节点，改为把这段前缀挂到它所通向的节点上当入边路径（§3.0），编进该节点哈希。

包裹的算法是：

    path_hash = keccak(path_info ‖ compressed_nibbles ‖ node_hash)

`node_hash` 是第一步的核心哈希，`path_hash` 就是包含入边路径后的节点哈希。其中：
- `path_info = 128 + 64 × skip_first + (nibble_count % 63)` — 单字节，编码压缩元数据。
- `skip_first`：如果入边路径起始于奇数深度（即第一个 nibble 独占半字节），则为 1，否则为 0。
- `compressed_nibbles`：将路径 nibble 打包为字节。若 `skip_first` 为 1，第一个 nibble 作为独立字节输出；其余两两配对（高 nibble、低 nibble）。

入边路径就这样进了哈希，所以压缩方式必须精确一致 —— 下面两个陷阱都源于此。

**陷阱 — `% 63` 而非 `% 64`。** 路径长度在 `path_info` 字节中按模 63 编码，这是 Conflux 特有的。标准实现可能假设模 64（6 位域）。搞错这一点会导致路径长度 ≥ 63 nibble 时的静默 root 偏离 — 实践中少见但在深层 storage key 上确实会发生。

**陷阱 — 奇偶对齐取决于深度而非 nibble 数。** 第一个 nibble 是否独立打包取决于 `start_depth % 2`，而不是 nibble 数量的奇偶。搞混这两者会导致压缩字节偏移半个字节，产生不同的哈希。

### 3.3 空 Trie 根

`MERKLE_NULL_NODE` 是空字节串的 keccak 哈希：

    MERKLE_NULL_NODE = keccak([]) = 0xc5d24601…

三段的根在 trie 为空时都用这个值。完全空状态的 state root 因此是 `keccak(MERKLE_NULL_NODE ‖ MERKLE_NULL_NODE ‖ MERKLE_NULL_NODE)`。

### 3.4 根节点不做路径压缩

根节点没有入边路径（它没有父节点），因此**不**做 §3.2 的包裹，根哈希就是它的核心哈希；只有子节点（depth > 0）才可能带入边路径。这是 Conflux MPT 的约定：根永不压缩，第一次分叉总是在全部 16 个 nibble 槽上进行。

---

## 4. 值与 Tombstone

### 4.1 MptValue

Delta 和 intermediate 中的每条记录要么是活跃值（`Some(bytes)`），要么是 tombstone（`Tombstone`）。这个区别对哈希有影响：

- `Some(bytes)` 按原字节参与哈希（节点哈希中的 `'v' ‖ value` 后缀）。
- `Tombstone` 按空字节切片参与哈希（节点得到 `'v'` 但后面无内容）。

**陷阱 — tombstone ≠ 不存在。** Delta 中的 tombstone 意味着"这个键被显式删除了，不要穿透到 intermediate 或 snapshot"。如果从 delta 中移除 tombstone 而不是保留它，被删除键在 snapshot 中的值就会重新出现。当 delta 为空时，节点*完全不带* `'v'` 后缀，其哈希与 tombstone 的空 `'v'` 是不同的。不存在、tombstone、活跃值 — 这三种状态在读取语义和哈希输出上都是不同的。

### 4.2 通过空值删除

将一个键设置为空字节切片被视为删除：实现会存储一个 `Tombstone` 而非零长度的 `Some`。调用者不需要在"设空"和"删除"之间选择 — 设空*就是*删除。

---

## 5. 前缀操作

前缀操作（读取与删除）的难点全在 delta 键空间。回忆 §2.2：delta 键里，**地址**和 **storage_key** 这两段各自先被整体哈希出一段 padding、再拼进键。要把一个前缀也翻成 delta 空间里的字节前缀，就得照搬这套编码——而这只有在前缀**没有切断这两段中任何一段**时才办得到（被哈希的字段必须完整，才能算出它的 padding）。据此分三种情形：

- **没切断任何被哈希的段 → 能正常编码。** 例如 account（完整地址）、`StorageRoot`（地址 ‖ `"data"`）、完整 storage 键，以及短地址（§2.3，本就原样透传、不哈希）。它们都是合法的 delta 前缀，扫描和删除都正常。
- **切断了地址 → `AddressPrefix`（§2.4）。** 它只是地址的一截，delta 算不出地址的 padding，编码器索性返回空向量，于是前缀匹配命中*全部* delta 条目，再事后按 canonical 地址过滤来补救。
- **切断了 storage_key → 部分 storage 键前缀。** 编码器拿残缺的 storage_key 去算 `storage_key_padding`，得到的 padding 与完整键不符，于是在 delta 里匹配不到任何条目——漏删，且无补救，是个 bug。

snapshot 层用 canonical 键（无 padding，§2.1），上述前缀在那里都是货真价实的字节前缀，一律正常；偏差只发生在 delta 和 intermediate（两者都带 padding）。后两种情形的具体后果见 §5.3。

### 5.1 前缀读取

`get_all_by_prefix` 按顺序扫描三层（delta → intermediate → snapshot），收集每个 canonical 键的首次出现。上层的 tombstone 会完全抑制该键 — 不包含在结果中。

对于 `AddressPrefix`，因其 delta 前缀为空（§5 开头），扫描会遍历*全部* delta/intermediate 条目，再按 canonical 地址过滤。其他前缀（如 `StorageRoot` 代表某地址下的所有 storage）的 delta 前缀有实际含义，能把扫描限定在相关范围内。

### 5.2 前缀删除

`delete_all_by_prefix` 必须处理三个层：

1. **Delta**：匹配的条目直接移除（它们是当前 period 的写入，移除是正确的）。
2. **Intermediate**：匹配的条目*不从* intermediate 移除（它逻辑上是只读的）。对每个可见的 intermediate 条目，通过 `set()` 在 delta 中插入一个 tombstone。
3. **Snapshot**：类似地，对每个匹配的 snapshot 条目，在 delta 中插入一个 tombstone。

**陷阱 — 前缀删除必须对下层插入 tombstone。** 曾经有 bug 只移除了 delta 条目，却没有为匹配的 intermediate 和 snapshot 条目插入 tombstone。这导致被删除的值在下一次读取时重新出现，因为读取在 delta 中没有找到条目时会穿透到下层。

**陷阱 — 前缀删除必须先清除 delta 条目。** 相关的另一个 bug 清除了 intermediate/snapshot 的条目，却忘了先移除匹配的 delta 条目。残留的 delta 条目遮蔽了后续的新写入。

### 5.3 Delta 键空间的前缀限制

§5 开头的后两种前缀（`AddressPrefix`、部分 storage 键）在 delta/intermediate 层都会出偏差，snapshot 不受影响。这里讲它们的实际后果。

**陷阱 — AddressPrefix 删除会清除无关的 delta 条目。** 它的 delta 前缀为空，前缀扫描命中*全部* delta 条目；实现先把这些条目都从 delta 移除，再按 canonical 地址过滤*返回值*。于是 `delete_all_by_prefix(AddressPrefix([1]))` 实际删光了 delta（不止地址 0x01… 的），尽管返回值只含地址匹配的那些。这是与上游一致的有意行为。

**陷阱 — 部分 storage 键前缀删不掉 delta/intermediate 条目（无法修复）。** 它在 delta 里匹配为空（原因见 §5 开头），于是 delta 和 intermediate 的目标条目删不掉，只有 snapshot 能删。典型触发是合约自毁清空 sponsor whitelist：白名单条目在 `SponsorWhitelistControl` 的 storage 下、storage_key 为 `被杀合约地址 ‖ 被赞助地址`，清空时用 `被杀合约地址` 这一截前缀（共识执行实际走这条，而非 `AddressPrefix`）。delta 编码方案下无解，唯一的规避是让条目跨过两个 snapshot 边界、迁到 snapshot 的 canonical 空间后再删（要两次旋转才彻底落到 snapshot）。

---

## 6. 合并（Snapshot 物化）

每个 period 边界，intermediate 条目被合并进 snapshot。这是一个重编码操作：每个 intermediate 条目使用 delta 键空间，必须先转换为 canonical 键空间再插入 snapshot。

转换路径是：`from_delta_mpt_key(raw_key)` → `StorageKeyWithSpace` → `to_key_bytes()` → canonical 键。

- 活跃的 intermediate 值按其 canonical 键插入 snapshot。
- tombstone 的 intermediate 值导致对应的 canonical 键从 snapshot 中移除。

合并后，必须重算新的 snapshot root。然后 intermediate 被替换为旧 delta，delta 清空，padding 从新的 root 重新计算。

**陷阱 — padding 依赖于新的 root。** 新的 delta padding 是 `keccak(new_snapshot_root ‖ new_intermediate_root)`，两个 root 必须完全计算出来后才能推导 padding。用旧 root 计算 padding 会导致后续所有键推导偏离。

**陷阱 — account-key 缓存必须随 padding 旋转。** 如果实现缓存了 `new_account_key` 的结果（以避免重复 keccak），缓存必须在 padding 变化时失效或旋转。旧的 delta 缓存可以复用为新的 intermediate 缓存（因为新 intermediate 的 padding 等于旧 delta 的 padding），但新的 delta 缓存必须从空开始。

---

## 7. Deposit/Vote 键编解码不对称

Delta 和 canonical 的解码器对 `DepositList` 和 `VoteList` 键的处理方式不同：

- **Delta 解码器**（`from_delta_mpt_key`）：使用 `rest.starts_with(prefix)`，接受并静默丢弃前缀之后的任何尾随字节。
- **Canonical 解码器**（`from_key_bytes`）：使用 `rest == prefix`，要求精确匹配。

目前这些键没有后缀，所以两个解码器一致。但如果未来添加后缀数据，delta→canonical 转换会静默丢弃它，导致合并时出现数据损坏。这一不对称是与上游保持兼容的有意选择，但也是潜在隐患。

---

## 8. 持久化与检查点

持久化的状态包含以下内容：

- **Snapshot**：canonical 键的映射，仅含活跃值，不含 tombstone。
- **Intermediate** 和 **Delta**：delta 键的映射，包含活跃值和 tombstone 两者。
- **Padding 字节**：intermediate 和 delta 各自的 32 字节 padding 数组。
- **Height**：epoch 计数（用于判断 period 边界）。
- **Last root**：最近一次的 `CommitRoot`（snapshot root、intermediate root、delta root、state root hash、padding）。

序列化 snapshot 时排除 tombstone（通过 `visible_value` 过滤）。Delta/intermediate 中的 tombstone 必须保留，因为它们承载着正确读取所需的删除语义。

---

## 9. 历史 Bug 汇总

以下每一条都是实现过程中实际遇到并通过链上验证确认修复的 bug。最后一条（#13）是个例外：它无法修复，按上游行为原样保留（见 §5.3）。

| # | Bug | 根因 | 所在层 |
|---|---|---|---|
| 1 | State root 被当作单棵 trie 的根 | 必须将三个根拼接后再哈希 | types |
| 2 | 空 delta root 与空 snapshot root 混淆 | 两者都是 `MERKLE_NULL_NODE`，但 state root 的组合方式不同 | trie |
| 3 | eSpace 标记在 delta 键中插入了错误的偏移 | Snapshot 用第 20 字节，delta 用第 32 字节 | key_codec |
| 4 | 假设 AddressPrefix 的 delta 前缀有实际含义 | 它始终是空的；先全量扫描再事后过滤 | key_codec / state |
| 5 | 前缀删除未对 snapshot 层插入 tombstone | 被删除的值在读穿透时重新出现 | state |
| 6 | 前缀删除未先清除 delta 层的条目 | 残留 delta 条目遮蔽了新写入 | state |
| 7 | Commit 在 period 边界未触发旋转 | Intermediate 和 snapshot 冻结，root 偏离 | state |
| 8 | 路径压缩用了 `% 64` 而非 `% 63` | 路径长度 ≥ 63 时发生静默偏离（深层 storage key） | trie |
| 9 | 路径压缩奇偶性基于 nibble 数而非深度 | 压缩字节偏移，产生不同哈希 | trie |
| 10 | 短地址键（< 20 字节）被送入 padding 哈希 | 必须原样透传，不做哈希 | key_codec |
| 11 | Delta 中的 tombstone 被移除而非保留 | Snapshot 中的值重新出现，删除丢失 | state |
| 12 | 合并后用旧 root 计算 padding | 后续所有键推导偏离 | state |
| 13 | 部分 storage_key 前缀删除删不掉 delta/intermediate 条目（无法修复） | `storage_key_padding` 依赖完整键，部分前缀算出的 padding 对不上，delta 前缀不可编码 | state / key_codec |
