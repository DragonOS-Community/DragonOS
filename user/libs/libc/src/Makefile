GARBAGE_PATTERNS := *.o *.s~ *.s *.S~ *.c~ *.h~ kernel
GARBAGE := $(foreach DIR,$(DIRS),$(addprefix $(DIR)/,$(GARBAGE_PATTERNS)))

all: libc

CFLAGS += -I .

libc_sub_dirs=math sys

ifeq ($(ARCH), __x86_64__)
libc_sub_dirs += sysdeps/x86_64
endif

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

libc: $(libc_objs) libc_rust
	@list='$(libc_sub_dirs)'; for subdir in $$list; do \
    		echo "make all in $$subdir";\
    		cd $$subdir;\
    		$(MAKE) all CFLAGS="$(CFLAGS) -I $(shell pwd)";\
    		cd ..;\
	done

libc_rust:
	rustup default nightly
	cargo +nightly build --release --target ./x86_64-unknown-none.json