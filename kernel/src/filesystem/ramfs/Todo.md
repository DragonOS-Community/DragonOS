# Todo

- [x] 将Keyer Struct level Wrapper转换到Entry method level，实现内置str比较器并存于BtreeSet中
     - Pro: 减少抽象开销，让上锁与引用更容易
     - Design: Entry::name_cmp(key: &str) -> Self
     - Boo: （在已经臃肿的结构里）多管理一个生命周期并不好

- [ ] 将Keyer的复制比较消除

- [ ] ~~为LockedEntry与LockedInode实现Deref Trait, 简化程序逻辑~~

- [ ] 为LockedEntry实现AsRef Trait, 简化比较逻辑

