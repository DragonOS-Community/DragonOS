echo "正在为rust换源"
echo "bash change_rust_src.sh --sparse以使用稀疏索引"
sparse="false"
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
    echo -e "[source.crates-io]   \n \
registry = \"https://github.com/rust-lang/crates.io-index\"  \n \
\n \
replace-with = 'tuna' \n \
[source.tuna] \n \
registry = \"sparse+https://mirrors.tuna.tsinghua.edu.cn/crates.io-index/\"	 \n \
" > ~/.cargo/config.toml
else
        echo -e "[source.crates-io]   \n \
registry = \"https://github.com/rust-lang/crates.io-index\"  \n \
\n \
replace-with = 'tuna' \n \
[source.tuna] \n \
registry = \"https://mirrors.tuna.tsinghua.edu.cn/git/crates.io-index.git\"	 \n \
" > ~/.cargo/config.toml

fi
