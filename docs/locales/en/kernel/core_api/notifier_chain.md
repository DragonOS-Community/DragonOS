:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: kernel/core_api/notifier_chain.md

- Translation time: 2025-05-19 01:41:30

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# Notifier Chain Notification Chain

## 1. Overview of Principles

&emsp;&emsp;The notification chain is an event notification mechanism between subsystems within the kernel or between modules within a subsystem. Essentially, a notification chain is a list of event handling functions. Each notification chain is associated with a specific type of event (e.g., reboot event). When a specific event occurs, the corresponding callback functions in the event's notification chain are called, allowing the subsystem/module to respond to the event and perform the appropriate processing.

&emsp;&emsp;The notification chain is somewhat similar to the subscription mechanism. It can be understood as: there is a "notifier" that maintains a list, and the "subscriber" registers its callback function into this list ("subscriber" can also unregister its callback function). When an event occurs that needs to be notified, the "notifier" traverses all the callback functions in the list and calls them, allowing all registered "subscribers" to respond and handle the event accordingly.

## 2. Core Features

### 2.1 Registering Callback Functions

&emsp;&emsp;The callback function is encapsulated into a specific structure and registered into the designated notification chain. The related method is `register`, which is used by the "subscriber".

### 2.2 Unregistering Callback Functions

&emsp;&emsp;The callback function is removed from the designated notification chain, i.e., it is deleted from the notification chain. The related method is `unregister`, which is used by the "subscriber".

### 2.3 Event Notification

&emsp;&emsp;When an event occurs, the notification chain related to that event performs the event notification through this method. `call_chain` This method traverses all elements in the notification chain and calls the registered callback functions in sequence. This method is used by the "notifier".

## 3. Types of Notification Chains

&emsp;&emsp;Each type of notification chain has corresponding `register`, `unregister`, and `call_chain` interfaces, with functions as described in the core features above.

- `AtomicNotifierChain`: Atomic notification chain, cannot sleep, recommended for use in interrupt context.
- `BlockingNotifierChain`: Blocking notification chain, can sleep, recommended for use in process context.
- `RawNotifierChain`: Raw notification chain, the caller is responsible for thread safety.

## 4. Other Issues

&emsp;&emsp;`BlockingNotifierChain` does not currently support the sleeping functionality.
