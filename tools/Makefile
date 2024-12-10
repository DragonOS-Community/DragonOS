.PHONY: fmt
fmt:
	@cargo fmt --all $(FMT_CHECK)

clean:
	@cargo clean

check:
	@cargo +nightly-2024-11-05 check --workspace --message-format=json
