ARCH ?= x86_64
self_dir := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))
mbedtls_version := 2.28.0
mbedtls_tarball := mbedtls_$(mbedtls_version).orig.tar.gz
mbedtls_tarball_path := $(mbedtls_tarball)
build_dir := build/$(ARCH)
mbedtls_dir := $(build_dir)/mbedtls-$(mbedtls_version)
mbedtls_build_dir := $(build_dir)/cmake-build
stage_dir := $(build_dir)/stage
host := $(ARCH)-linux-musl
prefix := $(host)-

cc := $(prefix)gcc
ar := $(prefix)ar
ranlib := $(prefix)ranlib
gcc_ar := $(prefix)gcc-ar
gcc_ranlib := $(prefix)gcc-ranlib
download_env := env -u http_proxy -u https_proxy -u HTTP_PROXY -u HTTPS_PROXY -u ALL_PROXY -u all_proxy

mbedtls_url_primary := http://archive.ubuntu.com/ubuntu/pool/universe/m/mbedtls/$(mbedtls_tarball)
mbedtls_url_fallback := https://github.com/Mbed-TLS/mbedtls/archive/refs/tags/mbedtls-$(mbedtls_version).tar.gz

cmake_args := \
	-G Ninja \
	-D CMAKE_SYSTEM_NAME=Linux \
	-D CMAKE_C_COMPILER=$(abspath $(shell command -v $(cc))) \
	-D CMAKE_C_COMPILER_AR=$(abspath $(shell command -v $(gcc_ar))) \
	-D CMAKE_C_COMPILER_RANLIB=$(abspath $(shell command -v $(gcc_ranlib))) \
	-D CMAKE_AR=$(abspath $(shell command -v $(ar))) \
	-D CMAKE_RANLIB=$(abspath $(shell command -v $(ranlib))) \
	-D CMAKE_C_FLAGS=-Os\ -fPIC \
	-D CMAKE_INSTALL_PREFIX=/usr \
	-D ENABLE_PROGRAMS=Off \
	-D ENABLE_TESTING=Off \
	-D USE_SHARED_MBEDTLS_LIBRARY=Off

$(mbedtls_tarball_path):
	@tmp_file="$@.tmp"; \
	rm -f "$$tmp_file"; \
	if ! $(download_env) curl -fL --retry 5 --retry-delay 1 -o "$$tmp_file" "$(mbedtls_url_primary)"; then \
		rm -f "$$tmp_file"; \
		$(download_env) curl -fL --retry 5 --retry-delay 1 -o "$$tmp_file" "$(mbedtls_url_fallback)"; \
	fi; \
	mv "$$tmp_file" "$@"

$(mbedtls_dir): $(mbedtls_tarball_path)
	mkdir -p $(build_dir)
	rm -rf $(mbedtls_dir)
	mkdir -p $(mbedtls_dir)
	tar -xzf $< -C $(mbedtls_dir) --strip-components=1
	cd $(mbedtls_dir) && patch -p1 < $(self_dir)/patches/0001-linux-musl-use-getrandom.patch

$(stage_dir)/usr/lib/libmbedtls.a: $(mbedtls_dir)
	mkdir -p $(mbedtls_build_dir)
	cd $(mbedtls_build_dir) && \
	cmake $(cmake_args) $(abspath $(mbedtls_dir))
	cd $(mbedtls_build_dir) && \
	cmake --build . -j$(nproc)
	rm -rf $(stage_dir)
	cd $(mbedtls_build_dir) && \
	DESTDIR=$(abspath $(stage_dir)) cmake --install .

.PHONY: all install clean distclean

all: $(stage_dir)/usr/lib/libmbedtls.a

install: all
	mkdir -p $(DADK_CURRENT_BUILD_DIR)/usr
	cp -a $(stage_dir)/usr/. $(DADK_CURRENT_BUILD_DIR)/usr/

clean:
	rm -rf build/

distclean: clean
	rm -f $(mbedtls_tarball_path)
