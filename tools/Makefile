.PHONY: fmt
fmt:
	@cargo fmt --all $(FMT_CHECK)

clean:
	@cargo clean

check:
	@cargo +nightly-2025-08-10 check --workspace --message-format=json
