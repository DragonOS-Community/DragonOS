CC=$(DragonOS_GCC)/x86_64-elf-gcc
LD=ld
OBJCOPY=objcopy

tmp_output_dir=$(ROOT_PATH)/bin/tmp/user
output_dir=$(DADK_BUILD_CACHE_DIR_TEST_DRSSTD_0_1_0)

all:
	mkdir -p $(tmp_output_dir)
	mkdir -p $(tmp_output_dir)/test_drsstd
	cargo -Z build-std=core,alloc,compiler_builtins install --target x86_64-unknown-dragonos --path .  --root $(tmp_output_dir)/test_drsstd
	mv $(tmp_output_dir)/test_drsstd/bin/test_drsstd $(output_dir)/test_drsstd
	rm -rf $(tmp_output_dir)/test_drsstd
clean:
	cargo clean