# BUGLOG

- 空 simple MPT root 是 keccak([])。
- 空 delta trie root 是 MERKLE_NULL_NODE。
- State root hash=keccak(三段root拼接)。
- delta AccountKey 前12字节是 padded hash。
- eSpace snapshot marker 在第20字节。
- eSpace delta marker 在第32字节。
- AddressPrefixKey delta prefix 是空 vec。
- 普通 prefix 用 delta 编码扫 trie。
- 老 MPT root 不等于简单 map hash。
- 手写压缩路径易错，需复刻 mask 语义。
- 压缩路径起始半字节由 depth 奇偶决定。
- MERKLE_NULL_NODE 是 keccak([])。
- AddressPrefix 删除会清掉 delta 全量。
- 短 AccountKey delta 编码保持原字节。
- Storage prefix 删除漏掉 delta 层。
- Storage prefix 删除命中 snapshot 层。
- commit 后未推进 delta/intermediate/snapshot。
