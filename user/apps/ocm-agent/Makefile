# SPDX-License-Identifier: GPL-2.0-only
#
# DragonOS build wrapper for ocm-agent.
#
# The upstream source is a git submodule pinned to
# https://github.com/sparkzky/ocm-agent.git (a mirror of
# gitee.com/OpenCloudOS/ocm-agent). Nothing from upstream is vendored here.
#
# Upstream builds a dynamically linked binary for a full Linux distribution.
# DragonOS ships a minimal root filesystem without a system dynamic loader,
# so the agent must be linked statically. CGO cannot be disabled because
# github.com/mattn/go-sqlite3 is used for the ongoing large-data store.

APP      := ocm-agent
SRC_DIR  := upstream
VERSION  ?= 0.1.0
COMMIT   ?= $(shell git -C $(SRC_DIR) rev-parse --short HEAD 2>/dev/null || echo unknown)
DATE     ?= $(shell date -u '+%Y-%m-%dT%H:%M:%SZ')
GO       ?= go
HAVE_GO  := $(shell command -v $(GO) >/dev/null 2>&1 && echo 1 || echo 0)
ARCH     ?= x86_64

ifeq ($(ARCH),x86_64)
	GOARCH := amd64
	MUSL_CC ?= $(shell command -v musl-gcc 2>/dev/null || command -v x86_64-linux-musl-gcc 2>/dev/null)
else ifeq ($(ARCH),riscv64)
	GOARCH := riscv64
	MUSL_CC ?= $(shell command -v riscv64-linux-musl-gcc 2>/dev/null || command -v musl-gcc 2>/dev/null)
else
$(error unsupported ARCH: $(ARCH))
endif

# Default to musl so the result never borrows the host's glibc. A static
# glibc build is acceptable but larger; musl matches the rest of the rootfs.
CC_STATIC ?= $(if $(MUSL_CC),$(MUSL_CC),gcc)

VERSION_LDFLAGS := \
	-X gitee.com/OpenCloudOS/ocmanager/tms/pkg/version.Version=$(VERSION) \
	-X gitee.com/OpenCloudOS/ocmanager/tms/pkg/version.Commit=$(COMMIT) \
	-X gitee.com/OpenCloudOS/ocmanager/tms/pkg/version.BuildDate=$(DATE)

GO_BUILD_FLAGS := -trimpath -buildvcs=false
GO_LDFLAGS := -s -w -linkmode external -extldflags "-static" $(VERSION_LDFLAGS)

ifeq ($(DADK_CURRENT_BUILD_DIR),)
	INSTALL_ROOT := ./install
else
	INSTALL_ROOT := $(DADK_CURRENT_BUILD_DIR)
endif
INSTALL_DIR := $(INSTALL_ROOT)/opt/ocm-agent

.PHONY: all build check-upstream test vet plugins clean install install-dadk install-plugins

all: build

check-upstream:
	@test -f $(SRC_DIR)/go.mod || { \
		echo "ERROR: upstream submodule is missing."; \
		echo "Run: git submodule update --init user/apps/ocm-agent/upstream"; \
		exit 1; \
	}

build: check-upstream
	@mkdir -p bin
	cd $(SRC_DIR) && CGO_ENABLED=1 GOOS=linux GOARCH=$(GOARCH) CC=$(CC_STATIC) \
		$(GO) build $(GO_BUILD_FLAGS) -ldflags '$(GO_LDFLAGS)' \
		-o $(CURDIR)/bin/$(APP) ./cmd/$(APP)
	@echo "built bin/$(APP) (static, upstream $(COMMIT))"

# Upstream unit tests run on the build host, not inside DragonOS.
test: check-upstream
	cd $(SRC_DIR) && $(GO) test ./... -short

vet: check-upstream
	cd $(SRC_DIR) && $(GO) vet ./...

# Optional helper collectors. They are usable on DragonOS only to the extent
# that the guest exposes the Linux procfs/sysfs files and tools they read.
# They are therefore opt-in and never part of the default image.
plugins: check-upstream
	@mkdir -p bin/plugins/os_monitor bin/plugins/sysinfo_collector bin/plugin-conf
	cd $(SRC_DIR)/plugins/os-monitor && CGO_ENABLED=0 $(GO) build -trimpath \
		-ldflags '-s -w' -o $(CURDIR)/bin/plugins/os_monitor/os_monitor .
	cp $(SRC_DIR)/plugins/os-monitor/app.conf.yml bin/plugins/os_monitor/
	cp $(SRC_DIR)/plugins/os-monitor/os_monitor.yml bin/plugin-conf/
	cd $(SRC_DIR)/plugins/sysinfo-collector && CGO_ENABLED=0 $(GO) build -trimpath \
		-ldflags '-s -w' -o $(CURDIR)/bin/plugins/sysinfo_collector/sysinfo_collector .
	cp $(SRC_DIR)/plugins/sysinfo-collector/sysinfo_collector.yml bin/plugin-conf/

install: build
	@mkdir -p $(INSTALL_DIR)/data $(INSTALL_DIR)/plugin-conf
	install -m755 bin/$(APP) $(INSTALL_DIR)/$(APP)
	install -m644 $(SRC_DIR)/config.yaml.example $(INSTALL_DIR)/config.yaml.example

install-plugins: plugins
	@mkdir -p $(INSTALL_DIR)/plugins $(INSTALL_DIR)/plugin-conf
	cp -r bin/plugins/* $(INSTALL_DIR)/plugins/
	cp bin/plugin-conf/*.yml $(INSTALL_DIR)/plugin-conf/

install-dadk:
ifeq ($(HAVE_GO),0)
	@echo "Go compiler not found; skipping ocm-agent build"
else
	$(MAKE) install
	@echo "installed $(INSTALL_DIR)/$(APP)"
endif

clean:
	rm -rf bin install
