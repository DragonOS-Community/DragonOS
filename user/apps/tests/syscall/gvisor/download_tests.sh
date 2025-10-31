#!/bin/bash

# gvisor系统调用测试套件下载和校验脚本
# 用于DragonOS项目

set -e

SCRIPT_DIR=$(dirname "$(realpath "$0")")
TESTS_DIR="$SCRIPT_DIR/tests"
TEST_ARCHIVE="gvisor-syscalls-tests.tar.xz"
TEST_URL="https://cnb.cool/DragonOS-Community/test-suites/-/releases/download/release_20250626/$TEST_ARCHIVE"
MD5SUM_URL="https://cnb.cool/DragonOS-Community/test-suites/-/releases/download/release_20250626/$TEST_ARCHIVE.md5sum"

# 颜色定义
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

# 检查命令是否存在
check_command() {
    if ! command -v "$1" &> /dev/null; then
        print_error "命令 '$1' 未找到，请安装相应软件包"
        exit 1
    fi
}

# 检查必要的命令
check_dependencies() {
    print_info "检查依赖..."
    check_command wget
    check_command md5sum
    check_command tar
}

# 检查测试套件是否已存在且完整
check_existing_tests() {
    if [ -d "$TESTS_DIR" ] && [ "$(find "$TESTS_DIR" -name "*_test" | wc -l)" -gt 0 ]; then
        print_info "发现已存在的测试套件"
        return 0
    else
        return 1
    fi
}

# 下载文件
download_file() {
    local url="$1"
    local output="$2"
    local quiet_mode="${3:-false}"
    
    if [ "$quiet_mode" = "true" ]; then
        # 静默下载（用于小文件如MD5）
        if wget -q -O "$output" "$url" 2>/dev/null; then
            return 0
        else
            return 1
        fi
    else
        # 显示进度的下载（用于大文件）
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

# 获取期望的MD5值
get_expected_md5() {
    # 将日志输出重定向到stderr，避免混入返回值
    print_info "获取期望的MD5校验和..." >&2
    local temp_md5_file=$(mktemp)
    
    # 确保在函数退出时清理临时文件
    trap "rm -f '$temp_md5_file'" RETURN
    
    if download_file "$MD5SUM_URL" "$temp_md5_file" "true"; then
        # 解析MD5文件，格式通常是: "md5hash filename"
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

# 验证MD5校验和
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

# 解压测试套件
extract_tests() {
    local archive="$1"
    
    print_info "解压测试套件..."
    mkdir -p "$TESTS_DIR"
    
    if tar -xf "$archive" -C "$TESTS_DIR" --strip-components=1; then
        print_info "解压完成"
        return 0
    else
        print_error "解压失败"
        return 1
    fi
}

# 清理临时文件
cleanup_temp_files() {
    print_info "清理临时文件..."
    rm -f "$SCRIPT_DIR/$TEST_ARCHIVE"
}

# 主函数
main() {
    local skip_if_exists=false
    local force_download=false

    # 检查参数
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

    print_info "开始检查和下载gvisor系统调用测试套件"

    # 检查依赖
    check_dependencies

    # 检查是否已存在测试套件
    if check_existing_tests; then
        if [ "$skip_if_exists" = true ]; then
            print_info "测试套件已存在，跳过下载"
            exit 0
        elif [ "$force_download" = true ]; then
            print_warn "强制重新下载，删除现有测试套件..."
            rm -rf "$TESTS_DIR"
        else
            read -p "测试套件已存在，是否重新下载？(y/N) " -n 1 -r
            echo
            if [[ ! $REPLY =~ ^[Yy]$ ]]; then
                print_info "使用现有测试套件"
                exit 0
            fi
            print_warn "删除现有测试套件..."
            rm -rf "$TESTS_DIR"
        fi
    fi
    
    # 下载测试套件
    if ! download_file "$TEST_URL" "$SCRIPT_DIR/$TEST_ARCHIVE"; then
        exit 1
    fi
    
    # 获取期望的MD5值
    local expected_md5
    if ! expected_md5=$(get_expected_md5); then
        cleanup_temp_files
        exit 1
    fi
    
    print_info "期望的MD5校验和: $expected_md5"
    
    # 验证MD5
    if ! verify_md5 "$SCRIPT_DIR/$TEST_ARCHIVE" "$expected_md5"; then
        cleanup_temp_files
        exit 1
    fi
    
    # 解压测试套件
    if ! extract_tests "$SCRIPT_DIR/$TEST_ARCHIVE"; then
        cleanup_temp_files
        exit 1
    fi
    
    # 清理临时文件
    cleanup_temp_files
    
    # 统计测试数量
    local test_count=$(find "$TESTS_DIR" -name "*_test" | wc -l)
    print_info "测试套件安装完成，共包含 $test_count 个测试用例"
    
    # 设置执行权限
    find "$TESTS_DIR" -name "*_test" -exec chmod +x {} \;
    
    print_info "gvisor测试套件准备就绪"
}

# 运行主函数
main "$@" 