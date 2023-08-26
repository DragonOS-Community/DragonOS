
all: 
	$(MAKE) CFLAGS="$(CFLAGS)" -C src all

clean:
	rm -f Cargo.lock
	$(MAKE) -C src clean

.PHONY: fmt
fmt:
	cargo fmt --all $(FMT_CHECK)
