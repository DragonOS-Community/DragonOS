# Internet Protocol Socket

众所都周之，这个 Inet Socket 常用的分为 TCP, UDP 和 ICMP。基于实用性，目前实现的是 TCP 和 UDP。

整个 Inet 网络协议栈与网卡的交互基于 `smoltcp` crate 来实现。

## Roadmap

- [ ] TCP
  - [x] 接受连接
  - [ ] 发起连接
  - [ ] 半双工关闭
- [x] UDP
  - [x] 传输数据
- [ ] ICMP
- [ ] ioctl
- [ ] Misc
  - [ ] 硬中断转软中断的锁处理（避免死锁）
  - [ ] epoll_item 优化
  - [ ] 优化 `inet port` 资源管理

## TCP

根据 TCP 状态机来 TCP Socket 的几个状态类
- `Init`: 裸状态
  - `Unbound`: 创建出来的状态
  - `Bound`: 绑定了地址
- `Listening`: 监听状态
- `Connecting`: 连接中状态
- `Established`: 连接建立状态

## UDP

UDP 是无连接的，所以没有连接状态。UDP 的状态只有 `Unbound` 和 `Bound` 两种。

## BoundInner

另一个对于 Inet Socket 的抽象，用于处理绑定网卡的 `socket`，从而封装 `smoltcp` 的接口，提供统一的资源管理。