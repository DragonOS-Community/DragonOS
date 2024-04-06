ifeq ($(ARCH), x86_64)
	CROSS_COMPILE=x86_64-linux-musl-
else ifeq ($(ARCH), riscv64)
	CROSS_COMPILE=riscv64-linux-musl-
endif

CC=$(CROSS_COMPILE)gcc


all: sender.c receiver.c
	$(CC) -static -o sender sender.c
	$(CC) -static -o receiver receiver.c

.PHONY: install clean
install: all
	mv sender $(DADK_CURRENT_BUILD_DIR)/sender
	mv receiver $(DADK_CURRENT_BUILD_DIR)/receiver

clean:
	rm receiver sender *.o

fmt:
