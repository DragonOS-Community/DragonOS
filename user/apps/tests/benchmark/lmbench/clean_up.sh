#! /bin/bash 
# 测试环境清理脚本
# 清理 lmbench 测试环境
# Usage: ./clean_up.sh

# 获取脚本所在目录
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)



# 清理测试文件
remove_test_files(){
    if [[ -f "/tmp/zero_file" ]]; then
        rm -f /tmp/zero_file
    fi
    if [[ -f "/tmp/test_file" ]]; then
        rm -f /tmp/test_file
    fi
}

# 清理 ext4 文件系统和镜像文件
clean_ext4_fs(){
    # 清理挂载的 ext4 文件系统和镜像文件
    if [[ -d "/ext4" ]]; then
        umount /ext4
        rm -rf /ext4
    fi
    if [[ -f "${SCRIPT_DIR}/ext4.img" ]]; then
        rm -f ${SCRIPT_DIR}/ext4.img
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

    remove_test_files
    clean_ext4_fs
}



# 执行清理
main "$@"

echo "Lmbench 测试环境已清理完成"
