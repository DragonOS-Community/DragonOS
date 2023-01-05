# binutils-2.38

## 说明

先修改build.sh中的路径，配置好需要的信息，再使用以下命令，即可开始编译：

```bash
bash build.sh
```

--- 

请注意，如果您要修改binutils的代码，请先使用以下命令，构建编辑binutils代码配置的环境：

```bash
docker build --no-cache -t dragonos-binutils-build .
```

然后再在binutils目录下执行以下命令，进入容器：

```bash
docker run --rm -it -v $PWD:/workdir -w /workdir dragonos-binutils-build
```