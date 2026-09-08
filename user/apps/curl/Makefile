ARCH ?= x86_64
curl_version := 8.5.0
curl_tarball := curl-$(curl_version).tar.xz
curl_tarball_path := $(curl_tarball)
build_dir := build/$(ARCH)
curl_dir := $(build_dir)/curl-$(curl_version)
host := $(ARCH)-linux-musl
prefix := $(host)-
bin := $(build_dir)/curl
mbedtls_root := $(abspath $(build_dir)/mbedtls-root)
mbedtls_prefix := $(mbedtls_root)/usr
ca_bundle_src := /etc/ssl/certs/ca-certificates.crt
ca_bundle_dst := etc/ssl/certs/ca-certificates.crt

cc := $(prefix)gcc
strip := $(prefix)strip
ar := $(prefix)ar
ranlib := $(prefix)ranlib
curl_url_primary := https://mirrors.dragonos.org.cn/pub/third_party/curl/$(curl_tarball)
curl_url_fallback := https://curl.se/download/$(curl_tarball)

configure_args := \
	--host=$(host) \
	--disable-shared \
	--enable-static \
	--disable-manual \
	--disable-ldap \
	--disable-ldaps \
	--disable-rtsp \
	--disable-dict \
	--disable-file \
	--disable-ftp \
	--disable-gopher \
	--disable-imap \
	--disable-mqtt \
	--disable-pop3 \
	--disable-smb \
	--disable-smtp \
	--disable-telnet \
	--disable-tftp \
	--disable-websockets \
	--without-openssl \
	--without-gnutls \
	--with-mbedtls=$(mbedtls_prefix) \
	--without-rustls \
	--without-bearssl \
	--without-wolfssl \
	--without-libpsl \
	--without-brotli \
	--without-zstd \
	--without-zlib \
	--with-ca-bundle=/$(ca_bundle_dst) \
	--without-ca-path

$(curl_tarball_path):
	@if ! env -u http_proxy -u https_proxy -u HTTP_PROXY -u HTTPS_PROXY -u ALL_PROXY -u all_proxy wget "$(curl_url_primary)"; then \
		rm -f "$(curl_tarball_path)"; \
		env -u http_proxy -u https_proxy -u HTTP_PROXY -u HTTPS_PROXY -u ALL_PROXY -u all_proxy wget "$(curl_url_fallback)"; \
	fi

$(curl_dir): $(curl_tarball_path)
	mkdir -p $(build_dir)
	tar -xJf $< -C $(build_dir)

$(mbedtls_prefix)/lib/libmbedtls.a:
	$(MAKE) -C ../mbedtls ARCH=$(ARCH) install DADK_CURRENT_BUILD_DIR=$(mbedtls_root)

$(bin): $(mbedtls_prefix)/lib/libmbedtls.a $(curl_dir)
	cd $(curl_dir) && \
	PKG_CONFIG=/bin/false \
	CC="$(cc)" AR="$(ar)" RANLIB="$(ranlib)" STRIP="$(strip)" \
	CPPFLAGS="-I$(mbedtls_prefix)/include" \
	CFLAGS="-Os -static" \
	LDFLAGS="-static -L$(mbedtls_prefix)/lib" \
	./configure $(configure_args)
	cd $(curl_dir) && \
	$(MAKE) CURL_LDFLAGS_BIN="-all-static" -j$(nproc)
	mkdir -p $(dir $(bin))
	cp $(curl_dir)/src/curl $(bin)
	$(strip) $(bin)

.PHONY: all install clean distclean

all: $(bin)

install: all
	mkdir -p $(DADK_CURRENT_BUILD_DIR)/bin
	cp $(bin) $(DADK_CURRENT_BUILD_DIR)/bin/curl
	mkdir -p $(DADK_CURRENT_BUILD_DIR)/$(dir $(ca_bundle_dst))
	cp $(ca_bundle_src) $(DADK_CURRENT_BUILD_DIR)/$(ca_bundle_dst)

clean:
	rm -rf build/

distclean: clean
	rm -f $(curl_tarball_path)
