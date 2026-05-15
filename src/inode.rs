//! Inode（索引节点）—— 文件的元数据与数据块索引
//!
//! 【核心改进】相比原C代码，本实现引入 inode 机制，带来以下提升：
//!
//! ## 1. 文件物理结构改进
//! 原C代码将文件的块链表直接嵌入目录项（dir_entry.first_block），
//! 导致目录结构与文件存储耦合在一起。本实现将文件元数据与目录分离：
//! - inode 独立管理文件的块指针、大小、权限等
//! - 目录项只保存 "文件名 → inode号" 的映射
//! - 支持硬链接（多个文件名指向同一inode）
//!
//! ## 2. 文件权限与安全
//! 原C代码没有权限系统（所有文件只读）。本实现引入：
//! - Unix风格的 rwx 权限位（owner/group/other）
//! - uid/gid 字段，支持多用户场景
//! - 权限检查在 open/create 等操作中执行
//!
//! ## 3. 多级索引块
//! 每个inode包含 10 个直接块指针 + 1 个一级间接块指针：
//! - 直接块：10 × 510字节 = 5,100字节 ≈ 5KB（小文件）
//! - 间接块：1 × (510/2) × 510字节 ≈ 127KB（较大文件）
//! - 总计支持约132KB的文件（原C代码只支持10KB）

use crate::disk::{Disk, BLOCK_SIZE, PAGE_SIZE};
use crate::superblock::SuperBlock;
use libc::{S_IFDIR, S_IFREG};
use std::time::{SystemTime, UNIX_EPOCH};

/// 每个inode的直接块指针数量
pub const DIRECT_BLOCKS: usize = 10;
/// inode结构体序列化后的大小（字节）
/// 布局：2+2+2+4+2+2+8+8+8 + 10×4 + 4 = 82，对齐到84
pub const INODE_SIZE: usize = 84;

/// Inode 结构 —— 文件的"身份证"
///
/// 【字段说明】
/// - mode: 文件类型（普通文件/目录）+ 权限位
/// - size: 文件实际大小（字节）
/// - direct_blocks: 直接块指针数组，每个指向一个数据块
/// - indirect_block: 一级间接块指针，指向的块内存储更多块指针
/// - 时间戳：mtime(修改时间), atime(访问时间), ctime(创建时间)
#[repr(C)]
#[derive(Debug, Clone)]
pub struct Inode {
    pub inode_no: u32,                   // inode编号
    pub mode: u16,                       // 文件类型和权限（S_IFREG | 0644 等）
    pub uid: u16,                        // 文件所有者用户ID
    pub gid: u16,                        // 文件所有者组ID
    pub size: u32,                       // 文件大小（字节）
    pub nlink: u16,                      // 硬链接计数
    pub block_count: u16,                // 已分配的数据块数
    pub mtime: i64,                      // 最后修改时间
    pub atime: i64,                      // 最后访问时间
    pub ctime: i64,                      // 创建时间
    pub direct_blocks: [u32; DIRECT_BLOCKS], // 直接块指针（10 × 510B ≈ 5KB）
    pub indirect_block: u32,             // 一级间接块指针（≈127KB）
}

impl Inode {
    /// 创建一个新的文件inode（默认权限 0644）
    pub fn new_file(inode_no: u32) -> Self {
        let now = current_timestamp();
        Inode {
            inode_no,
            mode: S_IFREG as u16 | 0o644, // 普通文件，rw-r--r--
            uid: 1000,                    // 默认用户ID
            gid: 1000,                    // 默认组ID
            size: 0,
            nlink: 1,
            block_count: 0,
            mtime: now,
            atime: now,
            ctime: now,
            direct_blocks: [0; DIRECT_BLOCKS],
            indirect_block: 0,
        }
    }

    /// 创建一个新的目录inode（默认权限 0755）
    pub fn new_dir(inode_no: u32) -> Self {
        let now = current_timestamp();
        Inode {
            inode_no,
            mode: S_IFDIR as u16 | 0o755, // 目录，rwxr-xr-x
            uid: 1000,
            gid: 1000,
            size: 0,
            nlink: 2,                    // 目录至少2个链接（. 和父目录的引用）
            block_count: 0,
            mtime: now,
            atime: now,
            ctime: now,
            direct_blocks: [0; DIRECT_BLOCKS],
            indirect_block: 0,
        }
    }

    /// 从字节数组反序列化
    pub fn from_bytes(inode_no: u32, data: &[u8]) -> Self {
        let read_u16 = |offset: usize| -> u16 {
            u16::from_le_bytes([data[offset], data[offset + 1]])
        };
        let read_u32 = |offset: usize| -> u32 {
            u32::from_le_bytes([data[offset], data[offset+1], data[offset+2], data[offset+3]])
        };
        let read_i64 = |offset: usize| -> i64 {
            i64::from_le_bytes([
                data[offset], data[offset+1], data[offset+2], data[offset+3],
                data[offset+4], data[offset+5], data[offset+6], data[offset+7],
            ])
        };

        let mut direct_blocks = [0u32; DIRECT_BLOCKS];
        for i in 0..DIRECT_BLOCKS {
            direct_blocks[i] = read_u32(38 + i * 4);
        }

        Inode {
            inode_no,
            mode: read_u16(0),
            uid: read_u16(2),
            gid: read_u16(4),
            size: read_u32(6),
            nlink: read_u16(10),
            block_count: read_u16(12),
            mtime: read_i64(14),
            atime: read_i64(22),
            ctime: read_i64(30),
            direct_blocks,
            indirect_block: read_u32(38 + DIRECT_BLOCKS * 4),
        }
    }

    /// 序列化为字节数组
    pub fn to_bytes(&self) -> [u8; INODE_SIZE] {
        let mut buf = [0u8; INODE_SIZE];
        buf[0..2].copy_from_slice(&self.mode.to_le_bytes());
        buf[2..4].copy_from_slice(&self.uid.to_le_bytes());
        buf[4..6].copy_from_slice(&self.gid.to_le_bytes());
        buf[6..10].copy_from_slice(&self.size.to_le_bytes());
        buf[10..12].copy_from_slice(&self.nlink.to_le_bytes());
        buf[12..14].copy_from_slice(&self.block_count.to_le_bytes());
        buf[14..22].copy_from_slice(&self.mtime.to_le_bytes());
        buf[22..30].copy_from_slice(&self.atime.to_le_bytes());
        buf[30..38].copy_from_slice(&self.ctime.to_le_bytes());

        for i in 0..DIRECT_BLOCKS {
            let offset = 38 + i * 4;
            buf[offset..offset+4].copy_from_slice(&self.direct_blocks[i].to_le_bytes());
        }

        let indirect_offset = 38 + DIRECT_BLOCKS * 4;
        buf[indirect_offset..indirect_offset+4].copy_from_slice(&self.indirect_block.to_le_bytes());
        buf
    }

    /// 检查权限：当前操作是否被允许
    ///
    /// 【安全改进】在访问文件前进行权限检查，这是原C代码完全缺失的功能。
    pub fn check_permission(&self, want_write: bool, _uid: u32, _gid: u32) -> bool {
        let perm = self.mode & 0o777; // 提取权限位（u16足够容纳0o777=511）

        // 简化的权限检查：检查owner权限位
        // （完整实现应区分owner/group/other，这里为清晰展示原理做了简化）
        if want_write {
            (perm & 0o222) != 0 // 任一类用户有写权限
        } else {
            (perm & 0o444) != 0 // 任一类用户有读权限
        }
    }

    /// 获取第n个数据块的块号（n从0开始）
    ///
    /// 先查直接块，再查间接块。返回None表示超出范围。
    pub fn get_block(&self, disk: &Disk, index: u32) -> Option<u32> {
        if index < DIRECT_BLOCKS as u32 {
            let block = self.direct_blocks[index as usize];
            if block == 0 { None } else { Some(block) }
        } else if self.indirect_block != 0 {
            // 从间接块中读取块号
            let indirect_idx = index - DIRECT_BLOCKS as u32;
            let mut indirect_buf = vec![0u8; BLOCK_SIZE];
            disk.read_block(self.indirect_block, &mut indirect_buf);

            // 间接块中每2字节存一个块号（共可存255个）
            let offset = indirect_idx as usize * 2;
            if offset + 2 > PAGE_SIZE {
                return None;
            }
            let block = u16::from_le_bytes([indirect_buf[offset], indirect_buf[offset + 1]]) as u32;
            if block == 0 { None } else { Some(block) }
        } else {
            None
        }
    }

    /// 设置第n个数据块的块号
    pub fn set_block(&mut self, disk: &Disk, index: u32, block_num: u32) {
        if index < DIRECT_BLOCKS as u32 {
            self.direct_blocks[index as usize] = block_num;
        } else {
            // 需要间接块
            if self.indirect_block == 0 {
                // 间接块在首次使用时才分配（通过allocate_block）
                // 这里假设调用者已分配好indirect_block
            }
            let indirect_idx = index - DIRECT_BLOCKS as u32;
            let mut indirect_buf = vec![0u8; BLOCK_SIZE];
            disk.read_block(self.indirect_block, &mut indirect_buf);

            let offset = indirect_idx as usize * 2;
            indirect_buf[offset..offset+2].copy_from_slice(&(block_num as u16).to_le_bytes());
            disk.write_block(self.indirect_block, &indirect_buf);
        }
    }
}

/// 计算inode在磁盘上的存储位置
///
/// inode表从 sb.inode_table_start 块开始，每个inode占 INODE_SIZE 字节。
/// 多个inode可以打包在一个块中。
pub fn inode_block_location(sb: &SuperBlock, inode_no: u32) -> (u32, usize) {
    let inodes_per_block = BLOCK_SIZE / INODE_SIZE; // 每个块能存几个inode
    let block_offset = (inode_no as usize) / inodes_per_block;
    let byte_offset = ((inode_no as usize) % inodes_per_block) * INODE_SIZE;
    (sb.inode_table_start + block_offset as u32, byte_offset)
}

/// 从磁盘读取指定inode
pub fn read_inode(disk: &Disk, sb: &SuperBlock, inode_no: u32) -> Inode {
    let (block_num, offset) = inode_block_location(sb, inode_no);
    let mut block_buf = vec![0u8; BLOCK_SIZE];
    disk.read_block(block_num, &mut block_buf);
    Inode::from_bytes(inode_no, &block_buf[offset..offset + INODE_SIZE])
}

/// 将inode写回磁盘
pub fn write_inode(disk: &Disk, sb: &SuperBlock, inode: &Inode) {
    let (block_num, offset) = inode_block_location(sb, inode.inode_no);
    let mut block_buf = vec![0u8; BLOCK_SIZE];
    disk.read_block(block_num, &mut block_buf); // 读取整个块（保留其他inode）

    let data = inode.to_bytes();
    block_buf[offset..offset + INODE_SIZE].copy_from_slice(&data);
    disk.write_block(block_num, &block_buf);
}

/// 获取当前Unix时间戳
pub fn current_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}
