# init something here
current_path=$PATH
current_pwd=$PWD

# It's recommended that your pwd is in /DragonOS/tools/. 
# 建议自行选择安装的位置
STRUCTURE="x86_64"  # 这里县则 x86_64 (64位)，而不是选择 i686 架构(32位)
INSTALL_POS="$HOME/opt/dragonos-gcc"
PREFIX="$INSTALL_POS/gcc-$STRUCTURE-unknown-none"
TARGET="$STRUCTURE-elf"
PATH="$PREFIX/bin:$PATH"
TARGET_GCC="$STRUCTURE-elf-gcc"
TARGET_LD="$STRUCTURE-elf-ld"
TARGET_AS="$STRUCTURE-elf-as"


# check: Don't install the gcc-toolchain in /root/*
if [ "${HOME:0:5}" = "/root" ]; then
    echo "不要把GCC交叉编译工具链安装在/root/目录下, 或者请不要使用sudo"
    echo "gcc交叉编译工具链默认安装在: /home/<your_usr_name>/opt/dragonos-gcc/"
    exit 0
fi

# 获取选项
KEEP_BINUTILS=False
KEEP_GCC=False
CHANGE_SOURCE=False
while [ True ]; do
	if [ ! -n "$1" ]; then
		break
	fi
	case "$1" in
		"--rebuild")	
			echo "清除${INSTALL_POS}目录下的所有信息"
            rm -rf "${INSTALL_POS}"
		;; 
		"--kb")
			KEEP_BINUTILS=True
		;;
		"--kg")
			KEEP_GCC=True
		;;
        "--cs")
            CHANGE_SOURCE=True
        ;;
        "--help")
            echo "脚本选项如下:"
            echo "--rebuild: 清除上一次安装的全部信息, 即删掉$INSTALL_POS目录下的所有内容, 然后重新构建gcc工具链."
            echo "--kg(keep-gcc): 您确保${STRUCTURE}-gcc已被编译安装, 本次调用脚本不重复编译安装gcc. 如果没有安装，脚本仍然会自动安装."
            echo "--kb(keep-binutils): 您确保binutils已被编译安装, 本次调用脚本不重复编译安装binutils. 如果没有安装，脚本仍然会自动安装."
            echo "--cs(change source): 如果包含该选项, 使用清华源下载gcc和binutils. 否则默认官方源."
		;;
		*)
			echo "不认识参数$1"
		;;
	esac
	shift 1
done

# 安装开始[提示]
echo -e "\033[35m [开始安装] \033[0m"
echo -e "\033[33m gcc交叉编译工具链默认安装在: /home/<your_usr_name>/opt/dragonos-gcc/ \033[0m"
sleep 0.3s  

# install prerequisited
# 注意texinfo和binutils的版本是否匹配
# 注意gmp/mpc/mpfr和gcc/g++的版本是否匹配
sudo apt-get install -y g++ gcc make texinfo libgmp3-dev libmpc-dev libmpfr-dev flex

# build the workspace
mkdir $HOME/opt
mkdir $INSTALL_POS
mkdir $PREFIX
cd $INSTALL_POS


# compile binutils
BIN_UTILS="binutils-2.39"
BIN_UTILS_TAR="${BIN_UTILS}.tar.gz"
if [[ ! -n "$(find $PREFIX/bin/ -name ${TARGET_LD})" && ! -n "$(find $PREFIX/bin/ -name ${TARGET_AS})" ]] || [ ! KEEP_BINUTILS ]; then
    if [ KEEP_BINUTILS ]; then
        echo -e "\033[35m 没有检测到 ${TARGET_LD} 或 没有检测到 ${TARGET_AS}, -kb参数无效 \033[0m"
        echo -e "\033[35m 开始安装binutils \033[0m"
        sleep 1s
    fi
    if [ ! -d "$BIN_UTILS" ]; then
        if [ ! -f "$BIN_UTILS_TAR" ]; then
            echo -e "\033[33m [提醒] 如果使用的是国外源, 下载时间可能偏久. 如果需要使用清华源, 请以输入参数--cs, 即: bash build_gcc_toolchain.sh --cs  \033[0m "
            if [ CHANGE_SOURCE ]; then
                # 国内源
                wget "https://mirrors.tuna.tsinghua.edu.cn/gnu/binutils/${BIN_UTILS_TAR}" -P "$INSTALL_POS"
            else
                # 官方网站
                wget https://ftp.gnu.org/gnu/binutils/${BIN_UTILS_TAR} -P "$INSTALL_POS"
            fi
        fi
        tar zxvf "$BIN_UTILS_TAR"
    fi
    # 开始编译 
    mkdir build-binutils
    cd build-binutils
    ../${BIN_UTILS}/configure --target=$TARGET --prefix="$PREFIX" --with-sysroot --disable-nls --disable-werror
    make -j $(nproc)
    make install
    cd ..
fi 

# compile GCC
GCC_FILE="gcc-11.3.0"
GCC_FILE_TAR="${GCC_FILE}.tar.gz"
if [ ! -n "$(find $PREFIX/bin/* -name $TARGET_GCC)" ] || [ ! KEEP_GCC ]; then
    if [ KEEP_GCC ]; then
        echo -e "\033[35m 没有检测到 $TARGET_GCC, -kg参数无效 \033[0m"
        echo -e "\033[35m 开始安装binutils \033[0m"
        sleep 1s
    fi
    if [ ! -d "$GCC_FILE" ]; then
        if [ ! -f "$GCC_FILE_TAR" ]; then
                echo -e "\033[33m [提醒] 如果使用的是国外源, 下载时间可能偏久. 如果需要使用清华源, 请以输入参数--cs, 即: bash build_gcc_toolchain.sh --cs  \033[0m "
                if [ CHANGE_SOURCE ]; then
                    # 国内源
                    wget "https://mirrors.tuna.tsinghua.edu.cn/gnu/gcc/${GCC_FILE}/${GCC_FILE_TAR}" -P "$INSTALL_POS"
                else
                    # 官方网站
                    wget "http://ftp.gnu.org/gnu/gcc/${GCC_FILE}/${GCC_FILE_TAR}" -P "$INSTALL_POS"
                fi
        fi
        tar zxvf "$GCC_FILE_TAR"
    fi
    # 开始编译安装
    mkdir build-gcc
    cd build-gcc
    ../${GCC_FILE}/configure --target=$TARGET --prefix="$PREFIX" --disable-nls --enable-languages=c,c++ --without-headers
    make all-gcc -j $(nproc)
    make all-target-libgcc -j $(nproc)
    make install-gcc -j $(nproc)
    make install-target-libgcc -j $(nproc)
    cd ..
fi


# update PATH
if [ -n "$(grep -F "export DragonOS_GCC=$PREFIX/bin/" $HOME/.bashrc)" ]; then 
	echo "[info] DragonOS_GCC has been in the "'$PATH'
else 
	echo 'export DragonOS_GCC='"$PREFIX"'/bin/' >> "$HOME/.bashrc"
	echo 'export PATH="$DragonOS_GCC:$PATH"'	>> "$HOME/.bashrc"
	echo "[info] Add DragonOS_GCC into PATH successfully."
fi
source "$HOME/.bashrc"


# final check
if [ -n "$(find $PREFIX/bin/* -name $TARGET_GCC)" ] &&
   [ -n "$(find $PREFIX/bin/* -name $TARGET_LD)" ] &&
   [ -n "$(find $PREFIX/bin/* -name $TARGET_AS)" ]; then
    echo -e "\033[42;37m [构建成功] Build Successfully. \033[0m"
else 	
    echo -e "\033[31m [错误] 未找到$STRUCTURE-elf-gcc, $STRUCTURE-elf-ld和$STRUCTURE-elf-as. \033[0m"
    echo -e "\033[31m [构建失败] 请尝试重新运行build_gcc_toolchain.sh, 或者查看输出，找到错误的原因. \033[0m"
fi

cd "$current_pwd"