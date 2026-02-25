.PHONY: fmt
fmt:
	@cargo fmt --all $(FMT_CHECK)

clean:
	@cargo clean

check:
	@cargo +nightly-2026-02-24 check --workspace --message-format=json
