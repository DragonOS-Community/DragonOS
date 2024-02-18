ifeq ($(ARCH), x86_64)
export PREFIX=x86_64-linux-musl-
else ifeq ($(ARCH), riscv64)
export PREFIX=riscv64-linux-musl-
endif


export CC=$(PREFIX)gcc

all: dmesg
	mv dmesg $(DADK_CURRENT_BUILD_DIR)

dmesg: main.c
	$(CC) -static -o dmesg main.c dmesg.c

clean:
	rm dmesg *.o