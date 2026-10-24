echo "app_normal_with_env: build"
# 判断CC环境变量是否为'abc-gcc'
if [ "$CC" != "abc-gcc" ]; then
    echo "CC is not abc-gcc"
    exit 1
else
    echo "[OK]: CC is abc-gcc"
fi

