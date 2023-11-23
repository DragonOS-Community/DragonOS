GARBAGE_PATTERNS := *.o *.s~ *.s *.S~ *.c~ *.h~ kernel
GARBAGE := $(foreach DIR,$(DIRS),$(addprefix $(DIR)/,$(GARBAGE_PATTERNS)))

all: libc

CFLAGS += -I .

libc_sub_dirs=math sys

ifeq ($(ARCH), x86_64)
libc_sub_dirs += arch/x86_64
endif

$(libc_sub_dirs): ECHO
	$(MAKE) -C $@ all CFLAGS="$(CFLAGS)" ASFLAGS="$(ASFLAGS) -I $(shell pwd)"

libc_objs:= $(shell find ./*.c)

ECHO:
	@echo "$@"


$(libc_objs): ECHO
	$(CC) $(CFLAGS) -c $@ -o $@.o

clean:
	cargo clean
	rm -rf $(GARBAGE)
	@list='$(libc_sub_dirs)'; for subdir in $$list; do \
		echo "Clean in dir: $$subdir";\
		cd $$subdir && $(MAKE) clean;\
		cd .. ;\
	done

libc: $(libc_objs) $(libc_sub_dirs) libc_rust

libc_rust:
	cargo +nightly-2023-01-21 build --release --target ./arch/x86_64/x86_64-unknown-none.json
