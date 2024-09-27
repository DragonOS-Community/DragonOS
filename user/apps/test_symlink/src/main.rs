use std::fs;
use std::os::unix::fs as unix_fs;
use std::io::{Error, Write};

fn create_test_file(filename: &str, content: &str) -> Result<(), Error> {
    let mut file = fs::File::create(filename)?;
    writeln!(file, "{}", content)?;
    Ok(())
}

fn print_file_content(filename: &str) -> Result<(), Error> {
    let content = fs::read_to_string(filename)?;
    println!("Content of {}: {}", filename, content);
    Ok(())
}

fn test_symlink(target: &str, linkname: &str) -> Result<(), Error> {
    println!("Creating symbolic link {} -> {}", linkname, target);

    // 创建符号链接
    match unix_fs::symlink(target, linkname){
        Ok(_) => println!("Symbolic link created successfully"),
        Err(e) => {
            println!("Failed to create symbolic link: {}", e);
            return Err(e)
        },
    }
    // unix_fs::symlink(target, linkname)?;

    // 验证符号链接
    let link_metadata = fs::symlink_metadata(linkname)?;
    if link_metadata.file_type().is_symlink() {
        println!("{} is a symlink", linkname);
    } else {
        println!("{} is not a symlink", linkname);
    }

    // 读取符号链接内容，应该等于目标文件路径
    let link_target = fs::read_link(linkname)?;
    println!("{} points to {}", linkname, link_target.display());

    // 验证符号链接是否指向了正确的目标文件
    print_file_content(linkname)?;

    Ok(())
}

fn main() -> Result<(), Error> {
    let target_filename = "test_target.txt";
    let symlink_filename = "test_symlink.txt";

    // 创建目标文件
    create_test_file(target_filename, "This is the target file for symlink")?;

    // 测试符号链接
    test_symlink(target_filename, symlink_filename)?;

    // 清理测试文件和符号链接
    fs::remove_file(target_filename)?;
    fs::remove_file(symlink_filename)?;

    Ok(())
}
