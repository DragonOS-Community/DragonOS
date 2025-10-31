:::{note}
**AI Translation Notice**

This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

- Source document: kernel/net/inet.md

- Translation time: 2025-09-11 16:37:18

- Translation model: `hunyuan-turbos-latest`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# Internet Protocol Socket

As is well known, the commonly used Inet Sockets are divided into TCP, UDP, and ICMP. For practical purposes, only TCP and UDP have been implemented so far.

The entire Inet network protocol stack interacts with the network card based on the `smoltcp` crate.

## Roadmap

- [ ] TCP
  - [x] Accept connections
  - [ ] Initiate connections
  - [ ] Half-duplex close
- [x] UDP
  - [x] Transmit data
- [ ] ICMP
- [ ] ioctl
- [ ] Misc
  - [ ] Lock handling for converting hardware interrupts to software interrupts (to avoid deadlocks)
  - [ ] epoll_item optimization
  - [ ] Optimize `inet port` resource management

## TCP

Several state classes of the TCP Socket are defined according to the TCP state machine:
- `Init`: Raw state
  - `Unbound`: State after creation
  - `Bound`: State after binding an address
- `Listening`: Listening state
- `Connecting`: Connecting state
- `Established`: Connected state

## UDP

UDP is connectionless, so there is no connection state. The UDP state only includes `Unbound` and `Bound`.

## BoundInner

Another abstraction for the Inet Socket, used to handle the `socket` bound to the network card, thereby encapsulating the `smoltcp` interface and providing unified resource management.
