#!/bin/bash
# lmbench 测试环境配置
# 在运行任何测试脚本前需要先 source 此文件
# Usage: source env.sh

# lmbench 二进制文件路径前缀
# 宿主机和 DragonOS 中路径一致
export LMBENCH_BIN="/lib/lmbench/bin/x86_64-linux-gnu"

# 测试数据目录
export LMBENCH_EXT2_DIR="/ext2"
export LMBENCH_TMP_DIR="/tmp"

# 日志目录
export LMBENCH_LOG_DIR="/tmp/lmbench_logs"

# 测试文件名
export LMBENCH_TEST_FILE="test_file"
export LMBENCH_ZERO_FILE="zero_file"

echo "======================================"
echo "Lmbench 环境变量已配置："
echo "  LMBENCH_BIN        = ${LMBENCH_BIN}"
echo "  LMBENCH_EXT2_DIR   = ${LMBENCH_EXT2_DIR}"
echo "  LMBENCH_TMP_DIR    = ${LMBENCH_TMP_DIR}"
echo "  LMBENCH_LOG_DIR    = ${LMBENCH_LOG_DIR}"
echo "======================================"
