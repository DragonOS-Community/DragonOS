use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;

fn main() {
    // 设置目标文件和符号链接的路径
    let target = "/myramfs/target_file.txt";
    let symlink_path = "/myramfs/another/symlink_file.txt";

    // 创建一个目标文件
    fs::write(target, "This is the content of the target file.")
        .expect("Failed to create target file");

    println!("Target file created successfully.");
    // 创建符号链接
    symlink(target, symlink_path).expect("Failed to create symlink");

    // 检查符号链接是否存在
    if Path::new(symlink_path).exists() {
        println!("Symlink created successfully.");
    } else {
        println!("Failed to create symlink.");
    }

    // 读取符号链接的内容
    let symlink_content = fs::read_link(symlink_path).expect("Failed to read symlink");
    println!("Symlink points to: {}", symlink_content.display());

    // 清理测试文件
    fs::remove_file(symlink_path).expect("Failed to remove symlink");
    fs::remove_file(target).expect("Failed to remove target file");
}
