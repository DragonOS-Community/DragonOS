# dropbear: https://matt.ucc.asn.au/dropbear/releases/dropbear-2025.88.tar.bz2

ARCH ?= x86_64
dropbear_version := 2025.88
dropbear_tarball := dropbear-$(dropbear_version).tar.bz2
dropbear_sha256 := 783f50ea27b17c16da89578fafdb6decfa44bb8f6590e5698a4e4d3672dc53d4
dropbear_tarball_path := $(dropbear_tarball)
build_dir := build/$(ARCH)
dropbear_dir := $(build_dir)/dropbear-$(dropbear_version)
prefix := $(ARCH)-linux-musl-
bin := build/$(ARCH)/dropbear

cc := $(prefix)cc
strip := $(prefix)strip

# 下载源码
.DELETE_ON_ERROR:
$(dropbear_tarball_path):
	wget -O "$@" https://matt.ucc.asn.au/dropbear/releases/$(dropbear_tarball)
	printf '%s  %s\n' '$(dropbear_sha256)' "$@" | sha256sum -c -
# 解压源码包
$(dropbear_dir): $(dropbear_tarball_path)
	mkdir -p $(build_dir)
	tar -xjf $< -C $(build_dir)

# 配置和编译
$(bin): $(dropbear_dir)
	cd $(dropbear_dir) && \
	./configure --host=$(prefix) CC=$(cc) --enable-static --disable-zlib --host=x86
	@# 执行编译
	cd $(dropbear_dir) && \
	make PROGRAMS="dropbear dbclient dropbearkey dropbearconvert" -j8
	@# 处理编译输出
	mkdir -p $(dir $(bin))
	cp $(dropbear_dir)/dropbear $(bin)
# 	$(strip) $(bin)
.PHONY: all clean menuconfig
all: $(bin)

install: all
	cp $(bin) $(DADK_CURRENT_BUILD_DIR)/dropbear

clean:
	rm -rf build
menuconfig:
	@echo "No menuconfig available for dropbear"

distclean: clean
	rm -f $(dropbear_tarball_path)
