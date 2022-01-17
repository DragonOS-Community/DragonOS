set(CMAKE_ASM_NASM_SOURCE_FILE_EXTENSIONS nasm asm S)
set(CMAKE_ASM_NASM_OBJECT_FORMAT bin)
set(CMAKE_ASM_NASM_FLAGS "-e")
set(CMAKE_ASM_NASM_LINK_EXECUTABLE "nasm <OBJECTS> -o <TARGET> <LINK_LIBRARIES>")
enable_language(ASM_NASM)

#修改输出的路径
set(EXECUTABLE_OUTPUT_PATH ${PROJECT_SOURCE_DIR}/bin/bootloader)
# 添加汇编包含目录（当前文件夹）
add_compile_options(-I ${CMAKE_CURRENT_SOURCE_DIR}/ )

add_executable(boot.bin boot.asm)
add_executable(loader.bin loader.asm fat12.inc)