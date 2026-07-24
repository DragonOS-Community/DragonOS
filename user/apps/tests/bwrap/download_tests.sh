#!/bin/bash

# bubblewrap测试套件下载和校验脚本
# 用于DragonOS项目

set -e

SCRIPT_DIR=$(dirname "$(realpath "$0")")
TESTS_DIR="$SCRIPT_DIR/tests"
TEST_VERSION="20260525"
TEST_ARCHIVE="bwrap-tests.tar.xz"
BASE_URL="https://cnb.cool/DragonOS-Community/test-suites/-/releases/download"
TEST_URL="$BASE_URL/release_${TEST_VERSION}/$TEST_ARCHIVE"
MD5SUM_URL="$BASE_URL/release_${TEST_VERSION}/$TEST_ARCHIVE.md5sum"
VERSION_FILE="$TESTS_DIR/.version"

GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

print_info() {
    echo -e "${GREEN}[INFO]${NC} $1"
}

print_warn() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

print_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

check_command() {
    if ! command -v "$1" > /dev/null 2>&1; then
        print_error "命令 '$1' 未找到，请安装相应软件包"
        exit 1
    fi
}

check_dependencies() {
    print_info "检查依赖..."
    check_command wget
    check_command md5sum
    check_command tar
}

check_existing_tests() {
    if [ -d "$TESTS_DIR" ]; then
        local current_version=""

        if [ -f "$VERSION_FILE" ]; then
            current_version=$(cat "$VERSION_FILE" 2>/dev/null || echo "")
        fi

        if [ -z "$current_version" ]; then
            print_warn "检测到未记录版本的测试套件，将升级到版本 $TEST_VERSION"
            rm -rf "$TESTS_DIR"
            return 1
        fi

        if [ "$current_version" != "$TEST_VERSION" ]; then
            print_warn "检测到旧版本测试套件 (当前: $current_version)，将升级到版本 $TEST_VERSION"
            rm -rf "$TESTS_DIR"
            return 1
        fi

        if [ -f "$TESTS_DIR/libtest.sh" ]; then
            print_info "发现已存在的测试套件，版本: $current_version"
            return 0
        fi
    fi

    return 1
}

download_file() {
    local url="$1"
    local output="$2"
    local quiet_mode="${3:-false}"

    if [ "$quiet_mode" = "true" ]; then
        if wget -q -O "$output" "$url" 2>/dev/null; then
            return 0
        else
            return 1
        fi
    else
        print_info "从 $url 下载文件..."
        if wget -q --show-progress -O "$output" "$url"; then
            print_info "下载完成: $output"
            return 0
        else
            print_error "下载失败: $url"
            return 1
        fi
    fi
}

get_expected_md5() {
    print_info "获取期望的MD5校验和..." >&2
    local temp_md5_file=$(mktemp)

    trap "rm -f '$temp_md5_file'" RETURN

    if download_file "$MD5SUM_URL" "$temp_md5_file" "true"; then
        local expected_md5=$(head -1 "$temp_md5_file" | cut -d' ' -f1)

        if [ -n "$expected_md5" ] && [ ${#expected_md5} -eq 32 ]; then
            echo "$expected_md5"
            return 0
        else
            print_error "MD5文件格式无效或为空" >&2
            return 1
        fi
    else
        print_error "无法下载MD5校验和文件" >&2
        return 1
    fi
}

verify_md5() {
    local file="$1"
    local expected="$2"

    print_info "验证MD5校验和..."
    local actual_md5=$(md5sum "$file" | cut -d' ' -f1)

    if [ "$actual_md5" = "$expected" ]; then
        print_info "MD5校验和验证成功"
        return 0
    else
        print_error "MD5校验和验证失败"
        print_error "期望: $expected"
        print_error "实际: $actual_md5"
        return 1
    fi
}

extract_tests() {
    local archive="$1"

    print_info "解压测试套件..."
    mkdir -p "$TESTS_DIR"

    if tar -xf "$archive" -C "$TESTS_DIR" --strip-components=1; then
        print_info "解压完成"
        if echo "$TEST_VERSION" > "$VERSION_FILE"; then
            print_info "记录测试套件版本: $TEST_VERSION"
            return 0
        else
            print_error "写入版本文件失败: $VERSION_FILE"
            return 1
        fi
    else
        print_error "解压失败"
        return 1
    fi
}

cleanup_temp_files() {
    print_info "清理临时文件..."
    rm -f "$SCRIPT_DIR/$TEST_ARCHIVE"
}

main() {
    local skip_if_exists=false
    local force_download=false

    for arg in "$@"; do
        case "$arg" in
            --skip-if-exists)
                skip_if_exists=true
                ;;
            --force-download)
                force_download=true
                ;;
        esac
    done

    print_info "开始检查和下载bubblewrap测试套件"

    check_dependencies

    if check_existing_tests; then
        if [ "$skip_if_exists" = true ]; then
            print_info "测试套件已存在，跳过下载"
            exit 0
        elif [ "$force_download" = true ]; then
            print_warn "强制重新下载，删除现有测试套件..."
            rm -rf "$TESTS_DIR"
        else
            printf "测试套件已存在，是否重新下载？(y/N) "
            read -r answer
            case "$answer" in
                [Yy]*)
                    print_warn "删除现有测试套件..."
                    rm -rf "$TESTS_DIR"
                    ;;
                *)
                    print_info "使用现有测试套件"
                    exit 0
                    ;;
            esac
        fi
    fi

    if ! download_file "$TEST_URL" "$SCRIPT_DIR/$TEST_ARCHIVE"; then
        exit 1
    fi

    local expected_md5
    if ! expected_md5=$(get_expected_md5); then
        cleanup_temp_files
        exit 1
    fi

    print_info "期望的MD5校验和: $expected_md5"

    if ! verify_md5 "$SCRIPT_DIR/$TEST_ARCHIVE" "$expected_md5"; then
        cleanup_temp_files
        exit 1
    fi

    if ! extract_tests "$SCRIPT_DIR/$TEST_ARCHIVE"; then
        cleanup_temp_files
        exit 1
    fi

    cleanup_temp_files

    local test_count=$(find "$TESTS_DIR" -name "*_test" 2>/dev/null | wc -l)
    print_info "测试套件安装完成，共包含 $test_count 个测试用例"

    find "$TESTS_DIR" -name "*_test" -exec chmod +x {} \; 2>/dev/null || true

    print_info "bwrap测试套件准备就绪"
}

main "$@"
