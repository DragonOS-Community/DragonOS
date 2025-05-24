:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: kernel/cpu_arch/x86_64/usb_legacy_support.md

- Translation time: 2025-05-19 01:41:19

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# USB Legacy Support

## Introduction

&emsp;&emsp;USB Legacy Support refers to the support provided by the BIOS for USB mice and USB keyboards. On computers that support and enable USB Legacy Support, the USB mouse and keyboard are simulated by the BIOS, making them appear to the operating system as if they were PS/2 mice and keyboards.

## Related

- When initializing the USB controller, its USB Legacy Support should be disabled.
