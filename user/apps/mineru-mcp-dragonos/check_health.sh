#!/bin/bash
# MinerU MCP DragonOS 健康检查脚本
# 用法: ./check_health.sh [port]

set -e

# 配置
PORT="${1:-3000}"
HOST="127.0.0.1"
URL="http://${HOST}:${PORT}/health"
TIMEOUT=5

# 颜色定义
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# 辅助函数
print_green() { printf '%b\n' "${GREEN}$1${NC}"; }
print_red() { printf '%b\n' "${RED}$1${NC}"; }
print_yellow() { printf '%b\n' "${YELLOW}$1${NC}"; }

echo "=== MinerU MCP DragonOS 健康检查 ==="
echo "目标: ${URL}"
echo ""

# 检查端口是否在监听
check_port() {
    ss -tlnp 2>/dev/null | grep -q ":${PORT} "
}

# 发送健康检查请求
check_health() {
    # 使用 --noproxy 绕过系统代理
    local response http_code body
    response=$(curl --noproxy '*' -s -w "\n%{http_code}" --max-time "${TIMEOUT}" "${URL}" 2>&1)
    http_code=$(echo "$response" | tail -n 1)
    body=$(echo "$response" | sed '$d')

    if [ "${http_code}" = "200" ]; then
        print_green "✓ HTTP 状态码: ${http_code}"
        echo ""
        echo "响应内容:"
        echo "$body" | python3 -m json.tool 2>/dev/null || echo "$body"
        echo ""
        validate_response "$body"
        return $?
    else
        print_red "✗ HTTP 状态码: ${http_code}"
        echo "响应: ${body}"
        return 1
    fi
}

# 验证响应内容
validate_response() {
    local body="$1"
    local errors=0

    echo "字段验证:"

    # 检查 status 字段
    local status
    status=$(echo "$body" | python3 -c "import sys,json; print(json.load(sys.stdin).get('status',''))" 2>/dev/null)
    if [ "$status" = "ok" ]; then
        print_green "  ✓ status: ok"
    else
        print_red "  ✗ status: ${status} (期望: ok)"
        ((errors++))
    fi

    # 检查 server 字段
    local server
    server=$(echo "$body" | python3 -c "import sys,json; print(json.load(sys.stdin).get('server',''))" 2>/dev/null)
    if [ "$server" = "mineru-mcp-dragonos" ]; then
        print_green "  ✓ server: ${server}"
    else
        print_yellow "  ? server: ${server} (期望: mineru-mcp-dragonos)"
    fi

    # 检查 api_mode 字段
    local api_mode
    api_mode=$(echo "$body" | python3 -c "import sys,json; print(json.load(sys.stdin).get('api_mode',''))" 2>/dev/null)
    if [ "$api_mode" = "local" ] || [ "$api_mode" = "remote" ]; then
        print_green "  ✓ api_mode: ${api_mode}"
    else
        print_red "  ✗ api_mode: ${api_mode} (期望: local 或 remote)"
        ((errors++))
    fi

    # 检查 has_api_key 字段
    local has_api_key
    has_api_key=$(echo "$body" | python3 -c "import sys,json; print(json.load(sys.stdin).get('has_api_key',''))" 2>/dev/null)
    if [ "$has_api_key" = "True" ] || [ "$has_api_key" = "true" ]; then
        print_green "  ✓ has_api_key: true"
    else
        print_yellow "  ? has_api_key: false"
    fi

    # 检查 version 字段
    local version
    version=$(echo "$body" | python3 -c "import sys,json; print(json.load(sys.stdin).get('version',''))" 2>/dev/null)
    if [ -n "$version" ]; then
        print_green "  ✓ version: ${version}"
    else
        print_red "  ✗ version: 缺失"
        ((errors++))
    fi

    return "$errors"
}

# 主逻辑
main() {
    # 1. 检查端口
    printf "检查端口 %s... " "$PORT"
    if check_port; then
        local pid
        pid=$(ss -tlnp 2>/dev/null | grep ":${PORT} " | grep -oP 'pid=\K[0-9]+' | head -1)
        print_green "已监听 (pid: ${pid:-unknown})"
    else
        print_red "未监听"
        echo ""
        print_red "错误: 服务未在端口 ${PORT} 启动"
        echo "请先启动服务: source .mineru.env && cargo run --release"
        exit 1
    fi
    echo ""

    # 2. 发送健康检查请求
    echo "发送健康检查请求..."
    if check_health; then
        echo ""
        print_green "========================================"
        print_green "✓ 健康检查通过"
        print_green "========================================"
        exit 0
    else
        echo ""
        print_red "========================================"
        print_red "✗ 健康检查失败"
        print_red "========================================"
        exit 1
    fi
}

main