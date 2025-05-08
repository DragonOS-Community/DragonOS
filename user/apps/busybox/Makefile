ARCH ?= x86_64
busybox_version := 1.35.0
busybox_tarball := busybox-$(busybox_version).tar.bz2
busybox_tarball_path := $(busybox_tarball)
build_dir := build/$(ARCH)
busybox_dir := $(build_dir)/busybox-$(busybox_version)
prefix := $(ARCH)-linux-musl-
bin := build/$(ARCH)/busybox

# 特殊架构处理
ifeq ($(ARCH), mipsel)
	prefix := mipsel-linux-musln32-
endif

cc := $(prefix)gcc
strip := $(prefix)strip

# 下载 busybox 的 md5sum 文件
$(busybox_tarball_path).md5sum:
	wget https://mirrors.dragonos.org.cn/pub/third_party/busybox/$(busybox_tarball).md5sum

# 下载源码
$(busybox_tarball_path): $(busybox_tarball_path).md5sum
	@if [ ! -f $@ ] || ! md5sum -c $(busybox_tarball_path).md5sum; then \
		echo "Downloading $@..."; \
		wget https://mirrors.dragonos.org.cn/pub/third_party/busybox/$(busybox_tarball); \
	fi

# 解压源码包
$(busybox_dir): $(busybox_tarball_path)
	mkdir -p $(build_dir)
	tar -xjf $< -C $(build_dir)

# 配置和编译
$(bin): $(busybox_dir)
	@# 应用必要补丁和配置调整
	cd $(busybox_dir) && \
	make defconfig && \
	sed -i '/CONFIG_STATIC/s/.*/CONFIG_STATIC=y/' .config && \
	sed -i '/CONFIG_PIE/d' .config && \
	sed -i '/CONFIG_FEATURE_EDITING/s/=y/=n/' .config && \
	sed -i '/CONFIG_HUSH/s/=y/=n/' .config  && \
	sed -i '/CONFIG_NOMMU/s/=y/=n/' .config && \
	echo "CONFIG_CROSS_COMPILER_PREFIX=\"$(prefix)\"" >> .config && \
	echo "CONFIG_FEATURE_STATIC=y" >> .config && \
	echo "CONFIG_STATIC_LIBGCC=y" >> .config && \
	echo "CONFIG_ASH=y" >> .config && \
	echo "CONFIG_ASH_READ_PROFILE=y" >> .config && \
	echo "CONFIG_FEATURE_EDITING=y" >> .config && \
	echo "CONFIG_HUSH=y" >> .config

	@# 执行编译
	cd $(busybox_dir) && \
	KCONFIG_NOTIMESTAMP=1 make CC="$(cc)" CFLAGS_EXTRA="-static -Os" LDFLAGS="--static" -j$(nproc)
	
	@# 处理编译输出
	mkdir -p $(dir $(bin))
	cp $(busybox_dir)/busybox $(bin)
	$(strip) $(bin)

.PHONY: all clean menuconfig

all: $(bin)

install: all
	mv $(bin) $(DADK_CURRENT_BUILD_DIR)/busybox

# 交互式配置菜单
menuconfig: $(busybox_dir)
	cd $(busybox_dir) && make menuconfig

clean:
	rm -rf build/

distclean: clean
	rm -f $(busybox_tarball_path) $(busybox_tarball_path).md5sum
