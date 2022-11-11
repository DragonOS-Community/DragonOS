# 更换Rust镜像源
echo -e "[source.crates-io]   \n \
registry = \"https://github.com/rust-lang/crates.io-index\"  \n \
\n \
replace-with = 'dragonos-gitee' \n \
[source.dragonos-gitee] \n \
registry = \"https://gitee.com/DragonOS/crates.io-index.git\"	 \n \
" > ~/.cargo/config