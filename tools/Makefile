.PHONY: fmt
fmt:
	@cargo fmt --all $(FMT_CHECK)

clean:
	@cargo clean

check:
	@cargo +nightly-2023-08-15 check --workspace --message-format=json
