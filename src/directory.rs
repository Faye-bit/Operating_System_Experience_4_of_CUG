//! 目录操作 —— 支持多级层次目录结构
//!
//! 【核心改进】这是相比原C代码最重要的改进之二。
//!
//! ## 原C代码的问题：平坦目录结构
//! 原实现只支持根目录（/），所有文件都存放在根目录下，不支持子目录。
//! 这种设计过于简化，无法满足实际文件系统的需求。
//!
//! ## 改进方案：层次目录
//! 本实现支持**任意深度的目录树**：
//! - 目录本身也是一个特殊文件（inode.mode中包含S_IFDIR标记）
//! - 目录的内容是一系列目录项（DirEntry），每个记录"文件名→inode号"的映射
//! - 支持 mkdir（创建子目录）、rmdir（删除空目录）
//! - 路径解析支持多级：/home/user/file.txt
//!
//! ## 目录项结构
//! 每个目录项记录：(inode号, 条目类型, 文件名长度, 文件名)
//! 条目类型区分普通文件和子目录，便于遍历和权限管理。

use crate::disk::{Disk, BLOCK_SIZE, PAGE_SIZE};
use crate::inode::{read_inode, write_inode, Inode};
use crate::superblock::SuperBlock;

/// 目录项的最大长度（inode + type + name_len + name）
pub const DIR_ENTRY_HEADER_SIZE: usize = 4 + 1 + 1; // inode(4) + entry_type(1) + name_len(1)
/// 单个文件名最大长度
pub const MAX_NAME_LEN: usize = 28; // 使每个条目32字节，块内对齐
/// 每个目录项的总大小
pub const DIR_ENTRY_SIZE: usize = DIR_ENTRY_HEADER_SIZE + MAX_NAME_LEN; // = 34，对齐到32用padding

/// 目录项 —— 目录文件中存储的最小记录单元
///
/// 目录文件的内容由一系列 DirEntry 组成。
/// 删除文件时只将其name_len置0（标记为删除），不移动其他条目。
#[repr(C)]
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub inode: u32,          // 指向的inode号
    pub entry_type: u8,      // 0=空闲槽位, 1=普通文件, 2=目录
    pub name_len: u8,        // 实际文件名长度
    pub name: [u8; MAX_NAME_LEN], // 文件名（UTF-8编码）
}

impl DirEntry {
    /// 创建目录项
    pub fn new(inode: u32, name: &str, is_dir: bool) -> Self {
        let name_bytes = name.as_bytes();
        let len = name_bytes.len().min(MAX_NAME_LEN);
        let mut name_arr = [0u8; MAX_NAME_LEN];
        name_arr[..len].copy_from_slice(&name_bytes[..len]);

        DirEntry {
            inode,
            entry_type: if is_dir { 2 } else { 1 },
            name_len: len as u8,
            name: name_arr,
        }
    }

    /// 判断该槽位是否空闲
    pub fn is_empty(&self) -> bool {
        self.name_len == 0
    }

    /// 获取文件名（作为&str）
    pub fn name_str(&self) -> &str {
        std::str::from_utf8(&self.name[..self.name_len as usize]).unwrap_or("???")
    }

    /// 从字节切片解析
    pub fn from_bytes(data: &[u8]) -> Self {
        let inode = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        DirEntry {
            inode,
            entry_type: data[4],
            name_len: data[5],
            name: data[6..6 + MAX_NAME_LEN].try_into().unwrap(),
        }
    }

    /// 序列化为字节数组
    pub fn to_bytes(&self) -> [u8; DIR_ENTRY_SIZE] {
        let mut buf = [0u8; DIR_ENTRY_SIZE];
        buf[0..4].copy_from_slice(&self.inode.to_le_bytes());
        buf[4] = self.entry_type;
        buf[5] = self.name_len;
        buf[6..6 + MAX_NAME_LEN].copy_from_slice(&self.name);
        buf
    }
}

/// 在目录中查找指定名字的条目
///
/// 遍历目录的所有数据块，搜索匹配的目录项。
/// 返回 (DirEntry, 所在块号, 块内偏移) 或 None。
pub fn dir_lookup(disk: &Disk, dir_inode: &Inode) -> Vec<(DirEntry, u32, usize)> {
    let mut entries = Vec::new();
    let count = dir_inode.block_count as u32;
    for i in 0..count {
        let block_num = match dir_inode.get_block(disk, i) {
            Some(b) => b,
            None => continue,
        };
        let mut block_buf = vec![0u8; BLOCK_SIZE];
        disk.read_block(block_num, &mut block_buf);

        let entries_per_block = PAGE_SIZE / DIR_ENTRY_SIZE;
        for j in 0..entries_per_block {
            let offset = j * DIR_ENTRY_SIZE;
            let entry = DirEntry::from_bytes(&block_buf[offset..offset + DIR_ENTRY_SIZE]);
            if !entry.is_empty() {
                entries.push((entry, block_num, offset));
            }
        }
    }
    entries
}

/// 在目录中搜索指定名字的条目（返回第一个匹配）
pub fn dir_find(disk: &Disk, dir_inode: &Inode, name: &str) -> Option<(DirEntry, u32, usize)> {
    let count = dir_inode.block_count as u32;
    for i in 0..count {
        let block_num = match dir_inode.get_block(disk, i) {
            Some(b) => b,
            None => continue,
        };
        let mut block_buf = vec![0u8; BLOCK_SIZE];
        disk.read_block(block_num, &mut block_buf);

        let entries_per_block = PAGE_SIZE / DIR_ENTRY_SIZE;
        for j in 0..entries_per_block {
            let offset = j * DIR_ENTRY_SIZE;
            let entry = DirEntry::from_bytes(&block_buf[offset..offset + DIR_ENTRY_SIZE]);
            if !entry.is_empty() && entry.name_str() == name {
                return Some((entry, block_num, offset));
            }
        }
    }
    None
}

/// 在目录中添加一个新条目
///
/// 【改进】按需扩展目录数据块。目录也是文件，其"文件内容"就是目录项列表。
/// 使用与原C代码类似的链式块管理，但通过inode操作统一管理。
pub fn dir_add_entry(
    disk: &Disk,
    sb: &mut SuperBlock,
    dir_inode: &mut Inode,
    name: &str,
    child_inode: u32,
    is_dir: bool,
) -> Result<(), &'static str> {
    // 检查是否已存在同名条目
    if dir_find(disk, dir_inode, name).is_some() {
        return Err("entry already exists");
    }

    let new_entry = DirEntry::new(child_inode, name, is_dir);
    let entry_bytes = new_entry.to_bytes();

    let count = dir_inode.block_count as u32;

    // 先在现有块中寻找空闲槽位
    for i in 0..count {
        let block_num = dir_inode.get_block(disk, i).unwrap();
        let mut block_buf = vec![0u8; BLOCK_SIZE];
        disk.read_block(block_num, &mut block_buf);

        let entries_per_block = PAGE_SIZE / DIR_ENTRY_SIZE;
        for j in 0..entries_per_block {
            let offset = j * DIR_ENTRY_SIZE;
            if DirEntry::from_bytes(&block_buf[offset..offset + DIR_ENTRY_SIZE]).is_empty() {
                // 找到空闲槽位
                block_buf[offset..offset + DIR_ENTRY_SIZE].copy_from_slice(&entry_bytes);
                disk.write_block(block_num, &block_buf);

                // 更新目录inode
                dir_inode.mtime = crate::inode::current_timestamp();
                write_inode(disk, sb, dir_inode);
                return Ok(());
            }
        }
    }

    // 现有块都已满，需要分配新块
    // 【注意】目录文件的块管理复用inode中的块指针机制
    // 这里使用链式附加的方式扩展目录
    if count < crate::inode::DIRECT_BLOCKS as u32 {
        // 分配新数据块给目录
        let new_block = crate::bitmap::Bitmap::allocate_block(disk, sb)
            .ok_or("no free blocks for directory expansion")?;

        let mut block_buf = vec![0u8; BLOCK_SIZE];
        block_buf[..DIR_ENTRY_SIZE].copy_from_slice(&entry_bytes);
        disk.write_block(new_block, &block_buf);

        dir_inode.direct_blocks[count as usize] = new_block;
        dir_inode.block_count += 1;
        dir_inode.size = (dir_inode.block_count as u32) * PAGE_SIZE as u32;
        dir_inode.mtime = crate::inode::current_timestamp();
        write_inode(disk, sb, dir_inode);
        Ok(())
    } else {
        Err("directory too large")
    }
}

/// 从目录中删除一个条目（标记为空闲）
pub fn dir_remove_entry(
    disk: &Disk,
    sb: &mut SuperBlock,
    dir_inode: &mut Inode,
    name: &str,
) -> Result<(), &'static str> {
    let (_entry, block_num, offset) = dir_find(disk, dir_inode, name)
        .ok_or("entry not found")?;

    let mut block_buf = vec![0u8; BLOCK_SIZE];
    disk.read_block(block_num, &mut block_buf);

    // 将条目标记为删除（name_len=0表示空闲）
    block_buf[offset + 5] = 0; // 清除 name_len
    disk.write_block(block_num, &block_buf);

    // 更新目录inode
    dir_inode.mtime = crate::inode::current_timestamp();
    write_inode(disk, sb, dir_inode);
    Ok(())
}

/// 解析路径，返回目标inode及其父目录inode
///
/// 【改进】支持多级路径解析，如 /home/user/file.txt。
/// 原C代码只支持一级路径（/filename）。
///
/// 返回 (目标inode, 父目录inode, 文件名部分)
pub fn path_resolve(
    disk: &Disk,
    sb: &SuperBlock,
    path: &str,
) -> Result<(Inode, Inode, String), &'static str> {
    if path == "/" {
        let root = read_inode(disk, sb, sb.root_inode);
        return Ok((root.clone(), root, String::new()));
    }

    // 分割路径
    let components: Vec<&str> = path
        .split('/')
        .filter(|c| !c.is_empty())
        .collect();

    if components.is_empty() {
        let root = read_inode(disk, sb, sb.root_inode);
        return Ok((root.clone(), root, String::new()));
    }

    // 从根目录开始逐级解析
    let mut current_inode = read_inode(disk, sb, sb.root_inode);

    for (idx, &component) in components.iter().enumerate() {
        if component.is_empty() || component.len() > MAX_NAME_LEN {
            return Err("invalid path component");
        }

        let parent_inode = current_inode.clone();

        let entry = dir_find(disk, &current_inode, component);
        match entry {
            Some((de, _, _)) => {
                let child = read_inode(disk, sb, de.inode);
                if idx == components.len() - 1 {
                    // 最后一级：返回目标
                    return Ok((child, parent_inode, component.to_string()));
                }
                // 中间路径必须是目录
                if (child.mode & libc::S_IFDIR as u16) == 0 {
                    return Err("not a directory");
                }
                current_inode = child;
            }
            None => {
                if idx == components.len() - 1 {
                    // 最后一级不存在——调用者可能要创建它
                    return Err("no such entry");
                }
                return Err("no such directory");
            }
        }
    }

    Err("path resolution failed")
}
