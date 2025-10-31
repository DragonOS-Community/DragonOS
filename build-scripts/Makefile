all:
	@cargo +nightly-2025-08-10 build --release -p gen_kallsyms
fmt:
	cargo fmt --all $(FMT_CHECK)
clean:
	@cargo clean
check:
	@cargo +nightly-2025-08-10 check --workspace $(CARGO_ZBUILD) --message-format=json
.PHONY: fmt