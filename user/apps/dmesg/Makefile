all: dmesg
	mv dmesg $(ROOT_PATH)/bin/sysroot/bin/

dmesg: main.c
	musl-gcc -static -o dmesg main.c dmesg.c