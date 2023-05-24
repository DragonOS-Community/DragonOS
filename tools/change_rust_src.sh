# 更换Rust镜像源
echo -e "[source.crates-io]   \n \
registry = \"https://github.com/rust-lang/crates.io-index\"  \n \
\n \
replace-with = 'tuna' \n \
[source.tuna] \n \
registry = \"https://mirrors.tuna.tsinghua.edu.cn/git/crates.io-index.git\"	 \n \
" > ~/.cargo/config