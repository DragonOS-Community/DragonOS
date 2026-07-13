ARCH ?= x86_64
ifeq ($(ARCH), x86_64)
	CROSS_COMPILE=x86_64-linux-musl-
else ifeq ($(ARCH), riscv64)
	CROSS_COMPILE=riscv64-linux-musl-
endif

CC=$(CROSS_COMPILE)gcc
CFLAGS := -Wall -O2 -static -lpthread

SRCS := $(wildcard *.c)
BINS := $(SRCS:.c=)

# 含有自己 Makefile 的子目录
SUBDIRS := $(patsubst %/Makefile,%,$(wildcard */Makefile))

all: $(BINS)
# 	@echo "src: $(SRCS)"
	@echo "bins: $(BINS)"
	@for dir in $(SUBDIRS); do $(MAKE) -C $$dir all ARCH=$(ARCH); done

# 依赖同目录下所有 .h，任意头文件变更都会触发重编
%: %.c $(wildcard *.h)
	$(CC) $(CFLAGS) $< -o $@

install: all
	@echo "Installing binaries to $(DADK_CURRENT_BUILD_DIR)/"
	mv $(BINS) $(DADK_CURRENT_BUILD_DIR)/
	@for dir in $(SUBDIRS); do \
		$(MAKE) -C $$dir install ARCH=$(ARCH) \
			DADK_CURRENT_BUILD_DIR=$(DADK_CURRENT_BUILD_DIR); \
	done

clean:
	rm -f $(BINS)
	@for dir in $(SUBDIRS); do $(MAKE) -C $$dir clean ARCH=$(ARCH); done

.PHONY: all install clean
