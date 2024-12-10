echo "正在为rust换源"

sparse="false"

CARGO_HOME=${CARGO_HOME:-~/.cargo}
CONFIG_FILE=$CARGO_HOME/config.toml
# 创建父目录
if [ ! -d ~/.cargo ]; then
    mkdir -p ~/.cargo
fi

while true; do
    if [ -z "$1" ]; then
	break;
    fi
    case "$1" in
	"--sparse")
	    echo "使用稀疏索引"
	    sparse=""
	    ;;
    esac
    shift 1
    done


if [ -z ${sparse} ]; then
echo -e "[source.crates-io]                             \n \
replace-with = 'rsproxy-sparse'                         \n \
[source.rsproxy]                                        \n \
registry = \"https://rsproxy.cn/crates.io-index\"       \n \
[source.rsproxy-sparse]                                 \n \
registry = \"sparse+https://rsproxy.cn/index/\"         \n \
[registries.rsproxy]                                    \n \
index = \"https://rsproxy.cn/crates.io-index\"          \n \
[net]                                                   \n \
git-fetch-with-cli = true                               \n \
" > $CONFIG_FILE
else
echo "TIPS: bash change_rust_src.sh --sparse以使用稀疏索引"

echo -e "[source.crates-io]                             \n \
replace-with = 'rsproxy'                                \n \
[source.rsproxy]                                        \n \
registry = \"https://rsproxy.cn/crates.io-index\"       \n \
[source.rsproxy-sparse]                                 \n \
registry = \"sparse+https://rsproxy.cn/index/\"         \n \
[registries.rsproxy]                                    \n \
index = \"https://rsproxy.cn/crates.io-index\"          \n \
[net]                                                   \n \
git-fetch-with-cli = true                               \n \
" > $CONFIG_FILE
fi
