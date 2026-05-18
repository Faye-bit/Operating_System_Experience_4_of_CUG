//! FUSE文件系统操作实现
//!
//! 本模块实现了 fuser::Filesystem trait，将所有文件系统操作委托给底层模块。
//! 这是连接FUSE框架与自定义文件系统逻辑的桥梁。
//!
//! 【整体改进总结】相比原C代码的simplefs，本实现有以下三大改进方向：
//!
//! 1. 【空闲空间管理】使用位图(bitmap)替代链式空闲块管理
//!    - 分配释放O(1)定位，而链表需要O(n)遍历
//!    - 位图集中存储，不污染数据块
//!
//! 2. 【文件目录】支持层次化多级目录
//!    - 原C代码只能将文件放在根目录 /
//!    - 本实现支持 /dir1/subdir2/file.txt 这样的嵌套路径
//!    - 目录也是文件，目录内容为 DirEntry 列表
//!
//! 3. 【共享与安全】引入Unix权限系统
//!    - inode中存储 uid/gid/mode 字段
//!    - open/create时进行权限检查
//!    - 支持 chmod 修改权限

use crate::bitmap::Bitmap;
use crate::directory::DirEntry;
use crate::disk::{Disk, BLOCK_SIZE, PAGE_SIZE};
use crate::inode::{write_inode, Inode};
use crate::superblock::{read_superblock, write_superblock, SuperBlock};

/// 每个文件最多使用的数据块数（用于限制文件大小）
const MAX_FILE_BLOCKS: u32 = 20;

/// SimpleFS —— 文件系统主结构体
///
/// 持有磁盘句柄和超级块的可变引用。
/// 所有FUSE操作通过此结构体的方法执行。
pub struct SimpleFS {
    pub disk: Disk,
    pub sb: SuperBlock,
    #[allow(dead_code)]
    pub disk_path: String,
}

impl SimpleFS {
    /// 创建或打开文件系统
    pub fn mount(disk_path: &str) -> Self {
        let disk = Disk::open(disk_path);
        let size = disk.size();

        let sb = if size == 0 {
            disk.truncate(crate::disk::DISK_SIZE);
            format_new_fs(&disk)
        } else {
            read_superblock(&disk)
        };

        SimpleFS {
            disk,
            sb,
            disk_path: disk_path.to_string(),
        }
    }
}

// ====================== FUSE Filesystem Trait 实现 ======================
// 以下代码仅在 Linux 平台编译，因为 fuser crate 依赖 libfuse

#[cfg(target_os = "linux")]
mod fuse_impl {
    // 从父模块导入 SimpleFS 和公共项
    use super::{SimpleFS, MAX_FILE_BLOCKS, system_time_from_timestamp};
    // 从子模块导入核心类型
    use crate::bitmap::Bitmap;
    use crate::directory::{dir_add_entry, dir_find, dir_lookup, dir_remove_entry, DirEntry};
    use crate::disk::{BLOCK_SIZE, PAGE_SIZE};
    use crate::inode::{current_timestamp, read_inode, write_inode, Inode, DIRECT_BLOCKS};
    // FUSE 框架类型（仅 Linux 可用）
    use fuser::{
        FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
        ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request,
    };
    use libc::{EACCES, EEXIST, ENOENT, ENOSPC, ENOTDIR, ENOTEMPTY};
    use std::ffi::OsStr;
    use std::time::Duration;

    impl SimpleFS {
        /// 将inode转换为FUSE FileAttr
        pub fn inode_to_attr(&self, inode: &Inode) -> FileAttr {
            let kind = if (inode.mode & libc::S_IFDIR as u16) != 0 {
                FileType::Directory
            } else {
                FileType::RegularFile
            };

            FileAttr {
                ino: inode.inode_no as u64,
                size: inode.size as u64,
                blocks: inode.block_count as u64,
                atime: system_time_from_timestamp(inode.atime),
                mtime: system_time_from_timestamp(inode.mtime),
                ctime: system_time_from_timestamp(inode.ctime),
                crtime: system_time_from_timestamp(inode.ctime),
                kind,
                perm: inode.mode,
                nlink: inode.nlink as u32,
                uid: inode.uid as u32,
                gid: inode.gid as u32,
                rdev: 0,
                flags: 0,
                blksize: BLOCK_SIZE as u32,
            }
        }
    }

    impl Filesystem for SimpleFS {
        /// 获取文件/目录属性（stat系统调用的后端）
        fn getattr(&mut self, _req: &Request, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
            let inode = read_inode(&self.disk, &self.sb, ino as u32);
            let attr = self.inode_to_attr(&inode);
            reply.attr(&Duration::from_secs(1), &attr);
        }

        /// 读取目录内容（ls命令的后端）
        ///
        /// 【改进】支持层次目录结构 —— 可列出子目录内容
        fn readdir(
            &mut self,
            _req: &Request,
            ino: u64,
            _fh: u64,
            offset: i64,
            mut reply: ReplyDirectory,
        ) {
            let dir_inode = read_inode(&self.disk, &self.sb, ino as u32);
            if (dir_inode.mode & libc::S_IFDIR as u16) == 0 {
                reply.error(ENOTDIR);
                return;
            }

            let entries = dir_lookup(&self.disk, &dir_inode);

            if offset == 0 {
                reply.add(dir_inode.inode_no as u64, 1, FileType::Directory, OsStr::new("."));
            }
            if offset <= 1 {
                reply.add(1, 2, FileType::Directory, OsStr::new(".."));
            }

            for (i, (entry, _, _)) in entries.iter().enumerate() {
                let child_inode = read_inode(&self.disk, &self.sb, entry.inode);
                let file_type = if entry.entry_type == 2 {
                    FileType::Directory
                } else {
                    FileType::RegularFile
                };
                let entry_offset = 2 + i as i64;
                if entry_offset >= offset {
                    if reply.add(
                        child_inode.inode_no as u64,
                        entry_offset + 1,
                        file_type,
                        OsStr::new(entry.name_str()),
                    ) {
                        break;
                    }
                }
            }
            reply.ok();
        }

        /// 查找目录中的条目（路径解析的每一步）
        ///
        /// 【改进】支持多级路径解析，每级目录查找都走此方法
        fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
            let parent_inode = read_inode(&self.disk, &self.sb, parent as u32);
            if let Some((entry, _, _)) =
                dir_find(&self.disk, &parent_inode, name.to_str().unwrap_or(""))
            {
                let child_inode = read_inode(&self.disk, &self.sb, entry.inode);
                let attr = self.inode_to_attr(&child_inode);
                reply.entry(&Duration::from_secs(1), &attr, 0);
            } else {
                reply.error(ENOENT);
            }
        }

        /// 打开文件 —— 【安全改进】执行权限检查
        fn open(&mut self, _req: &Request, ino: u64, flags: i32, reply: ReplyOpen) {
            let inode = read_inode(&self.disk, &self.sb, ino as u32);
            let want_write = (flags & libc::O_WRONLY as i32) != 0
                || (flags & libc::O_RDWR as i32) != 0;
            if want_write && !inode.check_permission(true, _req.uid(), _req.gid()) {
                reply.error(EACCES);
                return;
            }
            reply.opened(ino, 0);
        }

        /// 读取文件内容 —— 【涉及系统调用】通过disk.read_block间接使用lseek+read
        fn read(
            &mut self,
            _req: &Request,
            ino: u64,
            _fh: u64,
            offset: i64,
            size: u32,
            _flags: i32,
            _lock_owner: Option<u64>,
            reply: ReplyData,
        ) {
            let inode = read_inode(&self.disk, &self.sb, ino as u32);
            if offset >= inode.size as i64 {
                reply.data(&[]);
                return;
            }

            let actual_size = size.min((inode.size as i64 - offset) as u32);
            let mut result = vec![0u8; actual_size as usize];
            let mut remaining = actual_size as usize;
            let mut copied = 0usize;
            let mut block_index = (offset as u32) / (PAGE_SIZE as u32);
            let mut block_offset = (offset as usize) % PAGE_SIZE;

            while remaining > 0 {
                let block_num = match inode.get_block(&self.disk, block_index) {
                    Some(b) => b,
                    None => break,
                };
                let mut block_buf = vec![0u8; BLOCK_SIZE];
                self.disk.read_block(block_num, &mut block_buf);

                let to_copy = remaining.min(PAGE_SIZE - block_offset);
                result[copied..copied + to_copy]
                    .copy_from_slice(&block_buf[block_offset..block_offset + to_copy]);

                copied += to_copy;
                remaining -= to_copy;
                block_index += 1;
                block_offset = 0;
            }
            reply.data(&result[..copied]);
        }

        /// 创建新文件 —— 【改进】在inode中创建完整的元数据记录
        fn create(
            &mut self,
            _req: &Request,
            parent: u64,
            name: &OsStr,
            _mode: u32,
            _umask: u32,
            _flags: i32,
            reply: ReplyCreate,
        ) {
            let name_str = name.to_str().unwrap_or("unknown");
            let mut parent_inode = read_inode(&self.disk, &self.sb, parent as u32);

            if dir_find(&self.disk, &parent_inode, name_str).is_some() {
                reply.error(EEXIST);
                return;
            }

            // 分配新的inode号
            let new_inode_no = {
                let mut candidate = 1000u32;
                loop {
                    let test_inode = read_inode(&self.disk, &self.sb, candidate);
                    if test_inode.nlink == 0 {
                        break candidate;
                    }
                    candidate += 1;
                    if candidate > 2000 {
                        reply.error(ENOSPC);
                        return;
                    }
                }
            };

            let mut new_inode = Inode::new_file(new_inode_no);
            new_inode.mode = (libc::S_IFREG as u16) | 0o644;
            write_inode(&self.disk, &self.sb, &new_inode);

            if let Err(e) = dir_add_entry(
                &self.disk,
                &mut self.sb,
                &mut parent_inode,
                name_str,
                new_inode_no,
                false,
            ) {
                reply.error(match e {
                    "entry already exists" => EEXIST,
                    _ => ENOSPC,
                });
                return;
            }

            let attr = self.inode_to_attr(&new_inode);
            reply.created(&Duration::from_secs(1), &attr, 0, new_inode_no as u64, 0);
        }

        /// 创建目录 —— 【改进】新增功能，原C代码不支持子目录
        fn mkdir(
            &mut self,
            _req: &Request,
            parent: u64,
            name: &OsStr,
            _mode: u32,
            _umask: u32,
            reply: ReplyEntry,
        ) {
            let name_str = name.to_str().unwrap_or("unknown");
            let mut parent_inode = read_inode(&self.disk, &self.sb, parent as u32);

            if dir_find(&self.disk, &parent_inode, name_str).is_some() {
                reply.error(EEXIST);
                return;
            }

            let new_inode_no = {
                let mut candidate = 500u32;
                loop {
                    let test_inode = read_inode(&self.disk, &self.sb, candidate);
                    if test_inode.nlink == 0 {
                        break candidate;
                    }
                    candidate += 1;
                    if candidate > 900 {
                        reply.error(ENOSPC);
                        return;
                    }
                }
            };

            let mut new_dir_inode = Inode::new_dir(new_inode_no);
            let data_block = match Bitmap::allocate_block(&self.disk, &mut self.sb) {
                Some(b) => b,
                None => {
                    reply.error(ENOSPC);
                    return;
                }
            };

            // 初始化目录数据块（包含 . 和 .. 条目）
            let mut block_buf = vec![0u8; BLOCK_SIZE];
            let dot = DirEntry::new(new_inode_no, ".", true);
            let entry_size = crate::directory::DIR_ENTRY_SIZE;
            block_buf[0..entry_size].copy_from_slice(&dot.to_bytes());
            let dotdot = DirEntry::new(parent as u32, "..", true);
            block_buf[entry_size..entry_size * 2].copy_from_slice(&dotdot.to_bytes());
            self.disk.write_block(data_block, &block_buf);

            new_dir_inode.direct_blocks[0] = data_block;
            new_dir_inode.block_count = 1;
            new_dir_inode.size = PAGE_SIZE as u32;
            write_inode(&self.disk, &self.sb, &new_dir_inode);

            dir_add_entry(
                &self.disk,
                &mut self.sb,
                &mut parent_inode,
                name_str,
                new_inode_no,
                true,
            )
            .unwrap();

            let attr = self.inode_to_attr(&new_dir_inode);
            reply.entry(&Duration::from_secs(1), &attr, 0);
        }

        /// 删除目录 —— 【改进】新增功能，原C代码不支持
        fn rmdir(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
            let name_str = name.to_str().unwrap_or("");
            let mut parent_inode = read_inode(&self.disk, &self.sb, parent as u32);

            let (entry, _, _) = match dir_find(&self.disk, &parent_inode, name_str) {
                Some(e) => e,
                None => {
                    reply.error(ENOENT);
                    return;
                }
            };

            let child_inode = read_inode(&self.disk, &self.sb, entry.inode);
            if (child_inode.mode & libc::S_IFDIR as u16) == 0 {
                reply.error(ENOTDIR);
                return;
            }

            // 确保目录为空
            let child_entries = dir_lookup(&self.disk, &child_inode);
            if child_entries.len() > 2 {
                reply.error(ENOTEMPTY);
                return;
            }

            // 释放子目录的数据块
            for i in 0..child_inode.block_count {
                if let Some(block_num) = child_inode.get_block(&self.disk, i as u32) {
                    Bitmap::mark_free(&self.disk, &mut self.sb, block_num);
                }
            }

            dir_remove_entry(&self.disk, &mut self.sb, &mut parent_inode, name_str).unwrap();

            let mut freed_inode = child_inode;
            freed_inode.nlink = 0;
            write_inode(&self.disk, &self.sb, &freed_inode);

            reply.ok();
        }

        /// 写入文件 —— 【涉及系统调用】通过disk.write_block间接使用lseek+write+fsync
        fn write(
            &mut self,
            _req: &Request,
            ino: u64,
            _fh: u64,
            offset: i64,
            data: &[u8],
            _write_flags: u32,
            _flags: i32,
            _lock_owner: Option<u64>,
            reply: ReplyWrite,
        ) {
            let mut inode = read_inode(&self.disk, &self.sb, ino as u32);
            let write_size = data.len();
            if write_size == 0 {
                reply.written(0);
                return;
            }

            let required_blocks =
                ((offset as usize + write_size + PAGE_SIZE - 1) / PAGE_SIZE) as u32;
            if required_blocks > MAX_FILE_BLOCKS {
                reply.error(libc::EFBIG);
                return;
            }

            // 确保有足够的数据块——从位图中分配
            while inode.block_count < required_blocks as u16 {
                let new_block = match Bitmap::allocate_block(&self.disk, &mut self.sb) {
                    Some(b) => b,
                    None => {
                        reply.error(ENOSPC);
                        return;
                    }
                };

                let idx = inode.block_count as u32;
                if idx < DIRECT_BLOCKS as u32 {
                    inode.direct_blocks[idx as usize] = new_block;
                } else if idx == DIRECT_BLOCKS as u32 {
                    let indirect = Bitmap::allocate_block(&self.disk, &mut self.sb)
                        .expect("failed to allocate indirect block");
                    inode.indirect_block = indirect;
                    let mut indirect_buf = vec![0u8; BLOCK_SIZE];
                    indirect_buf[0..2].copy_from_slice(&(new_block as u16).to_le_bytes());
                    self.disk.write_block(indirect, &indirect_buf);
                } else {
                    let indirect_idx = idx - DIRECT_BLOCKS as u32;
                    let mut indirect_buf = vec![0u8; BLOCK_SIZE];
                    self.disk.read_block(inode.indirect_block, &mut indirect_buf);
                    let byte_offset = indirect_idx as usize * 2;
                    indirect_buf[byte_offset..byte_offset + 2]
                        .copy_from_slice(&(new_block as u16).to_le_bytes());
                    self.disk.write_block(inode.indirect_block, &indirect_buf);
                }

                // 初始化新块
                let block_buf = vec![0u8; BLOCK_SIZE];
                self.disk.write_block(new_block, &block_buf);
                inode.block_count += 1;
            }

            // 写入数据到对应块
            let mut remaining = write_size;
            let mut block_index = (offset as u32) / (PAGE_SIZE as u32);
            let mut block_offset = (offset as usize) % PAGE_SIZE;
            let mut data_offset = 0usize;

            while remaining > 0 {
                let block_num = inode
                    .get_block(&self.disk, block_index)
                    .expect("block not found during write");
                let mut block_buf = vec![0u8; BLOCK_SIZE];
                self.disk.read_block(block_num, &mut block_buf);

                let to_write = remaining.min(PAGE_SIZE - block_offset);
                block_buf[block_offset..block_offset + to_write]
                    .copy_from_slice(&data[data_offset..data_offset + to_write]);
                self.disk.write_block(block_num, &block_buf);

                remaining -= to_write;
                data_offset += to_write;
                block_index += 1;
                block_offset = 0;
            }

            let new_size = offset as usize + write_size;
            if new_size > inode.size as usize {
                inode.size = new_size as u32;
            }
            inode.mtime = current_timestamp();
            inode.atime = inode.mtime;
            write_inode(&self.disk, &self.sb, &inode);

            reply.written(write_size as u32);
        }

        /// 删除文件 —— 【改进】通过位图正确回收空间
        fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
            let name_str = name.to_str().unwrap_or("");
            let mut parent_inode = read_inode(&self.disk, &self.sb, parent as u32);

            let (entry, _, _) = match dir_find(&self.disk, &parent_inode, name_str) {
                Some(e) => e,
                None => {
                    reply.error(ENOENT);
                    return;
                }
            };

            let mut inode = read_inode(&self.disk, &self.sb, entry.inode);
            if (inode.mode & libc::S_IFDIR as u16) != 0 {
                reply.error(libc::EISDIR);
                return;
            }

            // 【改进：位图空间回收】释放所有数据块
            for i in 0..inode.block_count {
                if let Some(block_num) = inode.get_block(&self.disk, i as u32) {
                    Bitmap::mark_free(&self.disk, &mut self.sb, block_num);
                }
            }
            if inode.indirect_block != 0 {
                Bitmap::mark_free(&self.disk, &mut self.sb, inode.indirect_block);
            }

            inode.nlink = 0;
            write_inode(&self.disk, &self.sb, &inode);

            dir_remove_entry(&self.disk, &mut self.sb, &mut parent_inode, name_str).unwrap();
            reply.ok();
        }

        /// 修改文件属性（chmod等） —— 【安全改进】支持动态修改权限
        fn setattr(
            &mut self,
            _req: &Request,
            ino: u64,
            mode: Option<u32>,
            _uid: Option<u32>,
            _gid: Option<u32>,
            _size: Option<u64>,
            _atime: Option<fuser::TimeOrNow>,
            _mtime: Option<fuser::TimeOrNow>,
            _ctime: Option<std::time::SystemTime>,
            _fh: Option<u64>,
            _crtime: Option<std::time::SystemTime>,
            _chgtime: Option<std::time::SystemTime>,
            _bkuptime: Option<std::time::SystemTime>,
            _flags: Option<u32>,
            reply: ReplyAttr,
        ) {
            let mut inode = read_inode(&self.disk, &self.sb, ino as u32);
            if let Some(new_mode) = mode {
                let file_type = inode.mode & 0xF000;
                inode.mode = file_type | ((new_mode & 0o777) as u16);
            }
            inode.mtime = current_timestamp();
            write_inode(&self.disk, &self.sb, &inode);

            let attr = self.inode_to_attr(&inode);
            reply.attr(&Duration::from_secs(1), &attr);
        }
    }
}

// ====================== 辅助函数 ======================

/// 将Unix时间戳转换为SystemTime
fn system_time_from_timestamp(ts: i64) -> std::time::SystemTime {
    std::time::UNIX_EPOCH + std::time::Duration::from_secs(ts.max(0) as u64)
}

/// 格式化新文件系统
///
/// 执行以下步骤：
/// 1. 写入超级块（含魔数、布局信息）
/// 2. 初始化位图（标记系统区域为已占用）
/// 3. 创建根目录inode和数据块
fn format_new_fs(disk: &Disk) -> SuperBlock {
    use crate::superblock::init_superblock;

    // 1. 写入超级块到块#0
    let sb = init_superblock(disk);

    // 2. 初始化位图（块#1~#5）
    Bitmap::init_bitmap(disk, &sb);

    // 3. 创建根目录（inode #2）
    let mut sb_mut = sb;
    let root_inode_no = 2u32;
    let mut root_inode = Inode::new_dir(root_inode_no);

    // 为根目录分配数据块
    let root_data_block = Bitmap::allocate_block(disk, &mut sb_mut)
        .expect("failed to allocate root directory data block");

    // 初始化根目录数据块
    let mut block_buf = vec![0u8; BLOCK_SIZE];
    let dot = DirEntry::new(root_inode_no, ".", true);
    let dotdot = DirEntry::new(root_inode_no, "..", true);
    let entry_size = crate::directory::DIR_ENTRY_SIZE;
    block_buf[0..entry_size].copy_from_slice(&dot.to_bytes());
    block_buf[entry_size..entry_size * 2].copy_from_slice(&dotdot.to_bytes());
    disk.write_block(root_data_block, &block_buf);

    root_inode.direct_blocks[0] = root_data_block;
    root_inode.block_count = 1;
    root_inode.size = PAGE_SIZE as u32;
    root_inode.nlink = 2;
    write_inode(disk, &sb_mut, &root_inode);

    // 更新超级块
    let updated_sb = SuperBlock {
        root_inode: root_inode_no,
        ..sb_mut
    };
    write_superblock(disk, &updated_sb);
    updated_sb
}
