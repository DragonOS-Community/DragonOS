
all: 
	@if [ -z $$DragonOS_GCC ]; then echo "\033[31m  [错误]尚未安装DragonOS交叉编译器, 请使用tools文件夹下的build_gcc_toolchain.sh脚本安装  \033[0m"; exit 1; fi
	$(MAKE) -C src all
	

clean:
	rm -f Cargo.lock
	$(MAKE) -C src clean

.PHONY: fmt
fmt:
	cargo fmt --all $(FMT_CHECK)


check:
	cargo +nightly-2023-01-21 check --workspace --message-format=json --target ./src/arch/x86_64/x86_64-unknown-none.json
