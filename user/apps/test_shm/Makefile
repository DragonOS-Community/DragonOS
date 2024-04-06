ifeq ($(ARCH), x86_64)
	CROSS_COMPILE=x86_64-linux-musl-
else ifeq ($(ARCH), riscv64)
	CROSS_COMPILE=riscv64-linux-musl-
endif

CC=$(CROSS_COMPILE)gcc

all: sender.c receiver.c test_info.c
	$(CC) -static -o sender sender.c
	$(CC) -static -o receiver receiver.c
	$(CC) -static -o info test_info.c

.PHONY: install clean
install: all
	mv sender $(DADK_CURRENT_BUILD_DIR)/sender
	mv receiver $(DADK_CURRENT_BUILD_DIR)/receiver
	mv info $(DADK_CURRENT_BUILD_DIR)/info

clean:
	rm receiver sender info *.o

fmt:
