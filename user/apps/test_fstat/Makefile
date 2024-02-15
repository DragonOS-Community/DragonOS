ifeq ($(ARCH), x86_64)
	PREFIX=x86_64-linux-musl-
else ifeq ($(ARCH), riscv64)
	PREFIX=riscv64-linux-musl-
endif

CC=$(PREFIX)gcc


out: main.c
	$(CC) -static -o out main.c

.PHONY: install clean
install: out
	mv out $(DADK_CURRENT_BUILD_DIR)/test_fstat

clean:
	rm out *.o
