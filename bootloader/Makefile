all: boot.bin loader.bin

boot.bin: boot.asm
	nasm boot.asm -o ../bin/bootloader/boot.bin

loader.bin: loader.asm
	nasm loader.asm -o ../bin/bootloader/lodaer.bin


clean:
	rm -rf *.asm~ Makefile~
