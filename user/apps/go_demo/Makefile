# Simple Makefile for Go Demo

APP_NAME = go_demo
BINARY = bin/$(APP_NAME)

# Detect if Go compiler is available
HAVE_GO := $(shell command -v go >/dev/null 2>&1 && echo 1 || echo 0)


all: build

build:
	@echo "Building $(APP_NAME)..."
	@mkdir -p bin
	go build -o $(BINARY) main.go
	@echo "Build complete: $(BINARY)"

run: build
	@echo "Running $(APP_NAME)..."
	./$(BINARY)

clean:
	@echo "Cleaning..."
	rm -f ./$(BINARY)
	@echo "Clean complete"

install-dadk:
ifeq ($(HAVE_GO),1)
	@echo "Go compiler found. Building $(APP_NAME) ..."
	@if [ -z "$(DADK_CURRENT_BUILD_DIR)" ]; then \
		echo "Error: DADK_CURRENT_BUILD_DIR is not set or empty. Abort build."; \
		exit 1; \
	fi
	@$(MAKE) build
	@mv ./$(BINARY) $(DADK_CURRENT_BUILD_DIR)/
	@echo "$(APP_NAME) build completed."
else
	@echo "Go compiler not found. Skipping $(APP_NAME) build (exit 0)."
endif
