#!/bin/bash
# lmbench 测试环境配置
# 运行任何具体的测试脚本之前需要先运行此脚本初始化测试环境
# Usage: bash init.sh

# 获取脚本所在目录
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)


# 创建并挂载 ext4 文件系统到 /ext4 目录（用于 ext4_xxx 测试）  
create_ext4_fs(){
    if [[ ! -d "/ext4" ]]; then
        mkdir -p /ext4
        if [[ -f "${SCRIPT_DIR}/ext4.img" ]]; then
            rm -f ${SCRIPT_DIR}/ext4.img
        fi
        dd if=/dev/zero of=${SCRIPT_DIR}/ext4.img bs=1M count=1024
        mkfs.ext4 ${SCRIPT_DIR}/ext4.img
        mount -o loop ${SCRIPT_DIR}/ext4.img /ext4
    fi
}


# 创建测试所需的文件
create_test_file() {
    local ext4_zero_file_path=${LMBENCH_EXT4_DIR}/zero_file
    local ext4_test_file_path=${LMBENCH_EXT4_DIR}/test_file
    local tmp_zero_file_path=/tmp/zero_file
    local tmp_test_file_path=/tmp/test_file

    if [[ ! -f "${ext4_zero_file_path}" ]]; then
        touch ${ext4_zero_file_path}
        dd if=/dev/zero of=${ext4_zero_file_path} bs=1M count=512
        echo "创建零文件 ${ext4_zero_file_path} 完成"
    fi

    if [[ ! -f "${ext4_test_file_path}" ]]; then
        touch ${ext4_test_file_path}
        dd if=/dev/zero of=${ext4_test_file_path} bs=1M count=512
        echo "创建测试文件 ${ext4_test_file_path} 完成"
    fi

    if [[ ! -f "${tmp_zero_file_path}" ]]; then
        touch ${tmp_zero_file_path}
        dd if=/dev/zero of=${tmp_zero_file_path} bs=1M count=512
        echo "创建临时零文件 ${tmp_zero_file_path} 完成"
    fi

    if [[ ! -f "${tmp_test_file_path}" ]]; then
        touch ${tmp_test_file_path}
        dd if=/dev/zero of=${tmp_test_file_path} bs=1M count=512
        echo "创建临时测试文件 ${tmp_test_file_path} 完成"
    fi
}

# main execution function
main(){
    # 自动请求 root 权限
    if [[ $EUID -ne 0 ]]; then
    echo "需要root权限正在自动请求"
    exec sudo bash "$0" "$@"
    exit 1
    fi
    # 加载环境变量配置
    source ${SCRIPT_DIR}/env.sh
    # 创建 ext4 文件系统和测试文件
    create_ext4_fs
    create_test_file
}


# ================== Start the initialization process =================
main "$@"
if [[ $? -eq 0 ]]; then
    echo "lmbench 测试环境初始化完成"
else
    echo "lmbench 测试环境初始化失败"
fi

