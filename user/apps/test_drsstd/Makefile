CC=$(DragonOS_GCC)/x86_64-elf-gcc
LD=ld
OBJCOPY=objcopy

tmp_output_dir=$(ROOT_PATH)/bin/tmp/user
output_dir=$(DADK_BUILD_CACHE_DIR_TEST_DRSSTD_0_1_0)

all:
	mkdir -p $(output_dir)/test_drsstd
	cargo -Z build-std=core,alloc,compiler_builtins install --target ./target.json --path .  --root $(output_dir)/test_drsstd
	mv $(output_dir)/test_drsstd/bin/test_drsstd $(output_dir)/test_drsstd
	rm -rf $(output_dir)/test_drsstd
clean:
	cargo clean

fmt:
	cargo fmt