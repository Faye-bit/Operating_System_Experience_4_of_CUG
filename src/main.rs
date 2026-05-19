//! SimpleFS —— 基于FUSE的简单文件系统（Rust实现）
//!
//! ## 项目概述
//!
//! 在非 Linux 平台上，FUSE 相关代码不会被编译，因此会产生 dead_code 警告。
//! 以下属性在非 Linux 平台抑制这些预期的警告。
#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports, unused_variables, unused_mut))]
//! 这是一个在用户空间实现的简单文件系统，使用Rust语言编写，
//! 通过FUSE（Filesystem in Userspace）框架挂载到Linux系统。
//!
//! ## 核心改进（相比C版本 simplefs.c）
//! 1. **空闲空间管理**：使用位图（bitmap）替代链式空闲块管理
//! 2. **文件目录**：支持层次化多级目录结构
//! 3. **共享与安全**：引入Unix风格的权限系统
//!
//! ## 系统调用使用说明
//! 本程序通过libc crate使用以下系统调用：
//! - open() —— 打开磁盘映像文件
//! - read() —— 读取磁盘块
//! - write() —— 写入磁盘块
//! - lseek() —— 定位磁盘读写位置
//! - close() —— 关闭磁盘映像文件
//! - ftruncate() —— 调整磁盘映像大小
//! - fsync() —— 同步数据到磁盘
//! - fstat() —— 获取文件元数据
//!
//! 所有磁盘I/O操作均通过这些系统调用完成，不依赖标准库的高层文件API。

mod bitmap;
mod directory;
mod disk;
mod fs;
mod inode;
mod superblock;

/// 在 macOS 等非 Linux 平台上，只能测试核心逻辑编译，无法实际挂载 FUSE 文件系统。
/// 完整的 FUSE 功能需要在 Linux (Ubuntu) 上运行。
#[cfg(not(target_os = "linux"))]
fn main() {
    println!("=== SimpleFS Rust 实现 ===");
    println!();
    println!("核心模块编译成功！");
    println!();
    println!("模块列表:");
    println!("  - disk.rs        磁盘I/O层（libc系统调用）");
    println!("  - superblock.rs  超级块管理");
    println!("  - bitmap.rs      位图空闲空间管理 ★改进一");
    println!("  - inode.rs       inode结构与权限 ★改进二、三");
    println!("  - directory.rs   层次目录操作 ★改进二");
    println!("  - fs.rs          FUSE文件系统trait实现");
    println!();
    println!("磁盘布局:");
    println!("  块大小: {}B", crate::disk::BLOCK_SIZE);
    println!("  总块数: {}", crate::disk::MAX_BLOCKS);
    println!("  磁盘容量: {:.1}MB", crate::disk::DISK_SIZE as f64 / (1024.0 * 1024.0));
    println!();
    println!("改进要点:");
    println!("  1. 位图(bitmap)替代链式空闲块管理 —— O(1)分配/释放");
    println!("  2. 层次化目录结构 —— 支持 /a/b/c 多级路径");
    println!("  3. Unix权限系统 —— rwx权限位 + chmod支持");
    println!();
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("⚠  FUSE 挂载仅在 Linux 上支持");
    println!("   请在 Ubuntu 上运行: cargo build --release");
    println!("   然后: ./target/release/simplefs /tmp/mnt");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
}

/// Linux 平台入口 —— 实际挂载 FUSE 文件系统
#[cfg(target_os = "linux")]
fn main() {
    use fuser::MountOption;
    use std::env;

    // 解析命令行参数
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        eprintln!("用法: {} <挂载点> [FUSE选项...]", args[0]);
        eprintln!();
        eprintln!("示例:");
        eprintln!("  {} /tmp/mnt", args[0]);
        eprintln!("  {} -f /tmp/mnt          # 前台运行", args[0]);
        eprintln!("  {} -f -s /tmp/mnt      # 单线程模式", args[0]);
        eprintln!();
        eprintln!("首次运行时会在当前目录创建 disk.img 作为虚拟磁盘。");
        eprintln!("后续运行会加载已有的 disk.img。");
        std::process::exit(1);
    }

    let mount_point = args.last().unwrap();

    println!("[SimpleFS] 正在挂载文件系统到 {} ...", mount_point);
    println!("[SimpleFS] 磁盘映像: disk.img ({:.1}MB)",
             crate::disk::DISK_SIZE as f64 / (1024.0 * 1024.0));
    println!("[SimpleFS] 块大小: {}B, 总块数: {}",
             crate::disk::BLOCK_SIZE, crate::disk::MAX_BLOCKS);

    let filesystem = fs::SimpleFS::mount("disk.img");

    println!("[SimpleFS] 空闲块数: {}", filesystem.sb.free_blocks);
    println!("[SimpleFS] 改进特性:");
    println!("           - 位图空闲空间管理 (Bitmap)");
    println!("           - 层次化目录结构");
    println!("           - Unix权限系统");

    // 不传递任何特殊 MountOption，让 FUSE 使用默认行为
    let options: Vec<MountOption> = vec![];
    println!("[SimpleFS] 文件系统已就绪，在另一个终端中操作 {} 即可测试", mount_point);
    println!("[SimpleFS] 按 Ctrl+C 退出...");
    match fuser::mount2(filesystem, mount_point, &options) {
        Ok(()) => {
            // mount2 正常返回时才会执行到这里（通常不会，因为它是阻塞的）
        }
        Err(e) => {
            eprintln!(
                "[SimpleFS] 挂载失败: {}",
                e
            );
            eprintln!("[SimpleFS] 故障排查建议:");
            eprintln!("  1. 删除旧的 disk.img: rm disk.img");
            eprintln!("  2. 确保挂载点存在: mkdir -p {}", mount_point);
            eprintln!("  3. 如果挂载点卡住: fusermount -u {} 2>/dev/null; umount {} 2>/dev/null", mount_point, mount_point);
            eprintln!("  4. 确保 fuse 内核模块已加载: sudo modprobe fuse");
            std::process::exit(1);
        }
    }
}
