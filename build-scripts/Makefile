all:
	@cargo +nightly-2024-11-05 build --release -p gen_kallsyms
fmt:
	cargo fmt --all $(FMT_CHECK)
clean:
	@cargo clean
check:
	@cargo +nightly-2024-11-05 check --workspace $(CARGO_ZBUILD) --message-format=json
.PHONY: fmt