ifeq ($(ARCH), x86_64)
	CROSS_COMPILE=x86_64-linux-musl-
else ifeq ($(ARCH), riscv64)
	CROSS_COMPILE=riscv64-linux-musl-
endif

CC=$(CROSS_COMPILE)gcc

NAME_PREFIX=test_shm_

all: sender.c receiver.c test_info.c
	$(CC) -static -o $(NAME_PREFIX)sender sender.c
	$(CC) -static -o $(NAME_PREFIX)receiver receiver.c
	$(CC) -static -o $(NAME_PREFIX)info test_info.c

.PHONY: install clean
install: all
	mv $(NAME_PREFIX)sender $(DADK_CURRENT_BUILD_DIR)/
	mv $(NAME_PREFIX)receiver $(DADK_CURRENT_BUILD_DIR)/
	mv $(NAME_PREFIX)info $(DADK_CURRENT_BUILD_DIR)/

clean:
	rm $(NAME_PREFIX)receiver $(NAME_PREFIX)sender $(NAME_PREFIX)info *.o

fmt:
