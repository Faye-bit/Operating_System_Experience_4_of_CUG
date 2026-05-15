//! 超级块（Superblock）—— 文件系统的元数据核心
//!
//! 【改进说明】相比原C代码的超级块，本实现新增了：
//! 1. bitmap_blocks —— 记录位图占用的块数，支持更灵活的空闲空间管理
//! 2. inode_table_start / data_start —— 显式记录各区域起始位置，便于扩展
//! 3. 使用 to_le_bytes / from_le_bytes 进行序列化，确保跨平台兼容

use crate::disk::{Disk, BLOCK_SIZE, MAX_BLOCKS};

/// 文件系统魔数 —— 用于校验磁盘映像是否为合法格式
const MAGIC: &[u8; 16] = b"SIMPLEFS-RUST\0\0\0";

/// 超级块结构 —— 占据块#0，存储文件系统的全局元数据
///
/// 【布局说明】
/// 整个磁盘映像的布局如下：
/// ┌──────────┬──────────┬──────────┬───────────────┐
/// │ 超级块   │  位图区   │  inode表  │   数据块区     │
/// │ (块0)    │ (块1~N)  │ (N+1~M)  │  (M+1~20479)  │
/// └──────────┴──────────┴──────────┴───────────────┘
#[repr(C)]
#[derive(Debug, Clone)]
pub struct SuperBlock {
    pub magic: [u8; 16],        // 魔数：标识文件系统类型
    pub total_blocks: u32,      // 磁盘总块数
    pub free_blocks: u32,       // 当前空闲块数
    pub bitmap_blocks: u32,     // 位图区占用的块数
    pub inode_table_start: u32, // inode表起始块号
    pub inode_table_blocks: u32,// inode表占用的块数
    pub data_start: u32,        // 数据区起始块号
    pub root_inode: u32,        // 根目录的inode号
}

impl SuperBlock {
    /// 计算结构体在内存中的大小（序列化后占用的字节数）
    pub const SIZE: usize = 16 + 4 + 4 + 4 + 4 + 4 + 4 + 4; // = 44 bytes

    /// 从字节数组反序列化超级块
    pub fn from_bytes(data: &[u8]) -> Self {
        let mut magic = [0u8; 16];
        magic.copy_from_slice(&data[0..16]);

        let read_u32 = |offset: usize| -> u32 {
            u32::from_le_bytes([data[offset], data[offset+1], data[offset+2], data[offset+3]])
        };

        SuperBlock {
            magic,
            total_blocks:  read_u32(16),
            free_blocks:   read_u32(20),
            bitmap_blocks: read_u32(24),
            inode_table_start: read_u32(28),
            inode_table_blocks: read_u32(32),
            data_start:    read_u32(36),
            root_inode:    read_u32(40),
        }
    }

    /// 将超级块序列化为字节数组（用于写入磁盘块#0）
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        buf[0..16].copy_from_slice(&self.magic);
        buf[16..20].copy_from_slice(&self.total_blocks.to_le_bytes());
        buf[20..24].copy_from_slice(&self.free_blocks.to_le_bytes());
        buf[24..28].copy_from_slice(&self.bitmap_blocks.to_le_bytes());
        buf[28..32].copy_from_slice(&self.inode_table_start.to_le_bytes());
        buf[32..36].copy_from_slice(&self.inode_table_blocks.to_le_bytes());
        buf[36..40].copy_from_slice(&self.data_start.to_le_bytes());
        buf[40..44].copy_from_slice(&self.root_inode.to_le_bytes());
        buf
    }

    /// 验证魔数
    pub fn is_valid(&self) -> bool {
        &self.magic[..12] == b"SIMPLEFS-RUST"
    }

    /// 计算位图需要占用的块数
    ///
    /// 每个块（512字节）可以管理 512*8 = 4096 个块的分配状态。
    /// 对于10MB磁盘（20480个块），需要 ceil(20480/4096) = 5 个块存放位图。
    pub fn calc_bitmap_blocks() -> u32 {
        (MAX_BLOCKS + BLOCK_SIZE as u32 * 8 - 1) / (BLOCK_SIZE as u32 * 8)
    }
}

/// 初始化超级块 —— 仅在首次创建磁盘映像时调用
pub fn init_superblock(disk: &Disk) -> SuperBlock {
    let bitmap_blocks = SuperBlock::calc_bitmap_blocks();
    // inode表从位图之后开始
    let inode_table_start = 1 + bitmap_blocks;
    // 为简单起见，预留 200 个inode，每个inode 64字节，需要 ceil(200*64/512) = 25 块
    let inode_table_blocks = 25u32;
    let data_start = inode_table_start + inode_table_blocks;

    let sb = SuperBlock {
        magic: *MAGIC,
        total_blocks: MAX_BLOCKS,
        free_blocks: MAX_BLOCKS - data_start, // 数据区以外的块视为"已用"
        bitmap_blocks,
        inode_table_start,
        inode_table_blocks,
        data_start,
        root_inode: 0, // 将在格式化时设置
    };

    // 写入超级块到块#0
    let mut block_buf = vec![0u8; BLOCK_SIZE];
    let sb_bytes = sb.to_bytes();
    block_buf[..SuperBlock::SIZE].copy_from_slice(&sb_bytes);
    disk.write_block(0, &block_buf);

    sb
}

/// 从磁盘读取超级块
pub fn read_superblock(disk: &Disk) -> SuperBlock {
    let mut block_buf = vec![0u8; BLOCK_SIZE];
    disk.read_block(0, &mut block_buf);
    let sb = SuperBlock::from_bytes(&block_buf);
    if !sb.is_valid() {
        panic!("Invalid filesystem magic: {:?}", std::str::from_utf8(&sb.magic));
    }
    sb
}

/// 将超级块写回磁盘（当修改了free_blocks等字段时需要调用）
pub fn write_superblock(disk: &Disk, sb: &SuperBlock) {
    let mut block_buf = vec![0u8; BLOCK_SIZE];
    let sb_bytes = sb.to_bytes();
    block_buf[..SuperBlock::SIZE].copy_from_slice(&sb_bytes);
    disk.write_block(0, &block_buf);
}
