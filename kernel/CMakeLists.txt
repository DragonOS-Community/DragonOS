

#修改输出的路径
set(EXECUTABLE_OUTPUT_PATH ${PROJECT_SOURCE_DIR}/bin/kernel)
# 添加汇编包含目录（当前文件夹）
add_compile_options(-I ${CMAKE_CURRENT_SOURCE_DIR}/ )

add_library(${PROJECT_NAME}-arch
        head.S
        main.c)
add_executable(kernel.bin
        head.S main.c)
set_property(
        TARGET kernel.bin
        PROPERTY
        DEPENDS
        "link.lds"
)