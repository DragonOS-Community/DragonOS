# 将引导程序写入boot.img
dd if=bin/boot.bin of=bin/boot.img bs=512 count=1 conv=notrunc
qemu-system-x86_64 -s -S -m 2048 -fda bin/boot.img