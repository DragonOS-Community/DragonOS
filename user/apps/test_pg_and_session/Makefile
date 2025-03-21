ifeq ($(ARCH), x86_64)
	CROSS_COMPILE=x86_64-linux-musl-
else ifeq ($(ARCH), riscv64)
	CROSS_COMPILE=riscv64-linux-musl-
endif

CC=$(CROSS_COMPILE)gcc


all: main.c 
	$(CC) -static -o test_pg_and_session main.c

.PHONY: install clean
install: all
	mv test_pg_and_session $(DADK_CURRENT_BUILD_DIR)/test_pg_and_session

clean:
	rm test_pg_and_session *.o

fmt:
