# dropbear: https://matt.ucc.asn.au/dropbear/releases/dropbear-2025.88.tar.bz2

ARCH ?= x86_64
dropbear_version := 2025.88
dropbear_tarball := dropbear-$(dropbear_version).tar.bz2
dropbear_tarball_path := $(dropbear_tarball)
build_dir := build/$(ARCH)
dropbear_dir := $(build_dir)/dropbear-$(dropbear_version)
prefix := $(ARCH)-linux-musl-
bin := build/$(ARCH)/dropbear

cc := $(prefix)cc
strip := $(prefix)strip

# 下载源码
$(dropbear_tarball_path):
	wget https://mirrors.dragonos.org.cn/pub/third_party/dropbear/$(dropbear_tarball)
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
