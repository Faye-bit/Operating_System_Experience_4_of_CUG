//! 位图（Bitmap）空闲空间管理
//!
//! 【核心改进】这是相比原C代码最重要的改进之一。
//!
//! ## 原C代码的问题：链式空闲块管理
//! 原实现使用**隐式链表**管理空闲块 —— 每个空闲块内部存储"下一个空闲块的块号"。
//! 这种方式的缺点：
//! - 分配/释放必须遍历链表，O(n)时间复杂度
//! - 无法快速判断某个特定块是否空闲
//! - 块内数据被空闲链指针污染
//!
//! ## 改进方案：位图
//! 本实现使用**位图（bitmap）**管理空闲空间：
//! - 每个块用1个bit表示：0=空闲，1=已分配
//! - 分配/释放是O(1)的位操作
//! - 可以O(1)判断任意块的状态
//! - 位图集中存放，不污染数据块
//! - 这是现代文件系统（ext2/ext4等）普遍采用的方法
//!
//! ## 位图布局
//! 位图存储在超级块之后的连续块中。对于10MB磁盘（20480个块）：
//! - 需要 20480 bits = 2560 bytes = 5 blocks
//! - 位图区占据块#1到块#5

use crate::disk::{Disk, BLOCK_SIZE, MAX_BLOCKS};
use crate::superblock::{read_superblock, write_superblock, SuperBlock};

/// 位图管理器
///
/// 每个bit代表一个块是否被分配：
/// - bit = 0：块空闲
/// - bit = 1：块已分配
pub struct Bitmap;

impl Bitmap {
    /// 计算存放指定块的bit在哪个字节、哪个bit位
    /// 返回 (字节偏移, bit位掩码)
    #[inline]
    fn locate(block_num: u32) -> (usize, u8) {
        let byte_idx = (block_num / 8) as usize;
        let bit_idx = (block_num % 8) as u8;
        (byte_idx, 1u8 << bit_idx)
    }

    /// 🔍 检查指定块是否已被分配
    /// 返回值：true=已占用，false=空闲
    pub fn is_allocated(disk: &Disk, _sb: &SuperBlock, block_num: u32) -> bool {
        if block_num >= MAX_BLOCKS {
            return true; // 越界视为已占用
        }

        let (byte_idx, mask) = Self::locate(block_num);
        let bitmap_block_offset = byte_idx / BLOCK_SIZE; // 在第几个位图块中
        let byte_offset_in_block = byte_idx % BLOCK_SIZE; // 在该块中的字节偏移

        let block_to_read = 1 + bitmap_block_offset as u32; // 位图从块#1开始
        let mut block_buf = vec![0u8; BLOCK_SIZE];
        disk.read_block(block_to_read, &mut block_buf);

        (block_buf[byte_offset_in_block] & mask) != 0
    }

    /// 📝 标记一个块为"已分配"
    pub fn mark_allocated(disk: &Disk, sb: &mut SuperBlock, block_num: u32) {
        let (byte_idx, mask) = Self::locate(block_num);
        let bitmap_block_offset = byte_idx / BLOCK_SIZE;
        let byte_offset_in_block = byte_idx % BLOCK_SIZE;

        let block_to_write = 1 + bitmap_block_offset as u32;
        let mut block_buf = vec![0u8; BLOCK_SIZE];
        disk.read_block(block_to_write, &mut block_buf);

        block_buf[byte_offset_in_block] |= mask; // 设置bit为1（已占用）
        disk.write_block(block_to_write, &block_buf);

        // 更新超级块中的空闲块计数
        sb.free_blocks -= 1;
        write_superblock(disk, sb);
    }

    /// 🗑️ 标记一个块为"空闲"
    pub fn mark_free(disk: &Disk, sb: &mut SuperBlock, block_num: u32) {
        let (byte_idx, mask) = Self::locate(block_num);
        let bitmap_block_offset = byte_idx / BLOCK_SIZE;
        let byte_offset_in_block = byte_idx % BLOCK_SIZE;

        let block_to_write = 1 + bitmap_block_offset as u32;
        let mut block_buf = vec![0u8; BLOCK_SIZE];
        disk.read_block(block_to_write, &mut block_buf);

        block_buf[byte_offset_in_block] &= !mask; // 清除bit（置0 = 空闲）
        disk.write_block(block_to_write, &block_buf);

        // 更新超级块
        sb.free_blocks += 1;
        write_superblock(disk, sb);
    }

    /// 🔎 寻找并分配第一个空闲块
    ///
    /// 从头扫描位图，找到第一个bit为0的块，将其标记为已分配并返回块号。
    /// 相比原C代码的链表遍历（O(n)），位图扫描虽然也是O(n)但常数小得多，
    /// 且在实现中可以利用块内64位一次比较8字节的优化。
    ///
    /// 返回值：分配到的块号，若无空闲块返回None
    pub fn allocate_block(disk: &Disk, sb: &mut SuperBlock) -> Option<u32> {
        if sb.free_blocks == 0 {
            return None;
        }

        let sb_local = read_superblock(disk);
        let bitmap_blocks = sb_local.bitmap_blocks as usize;

        // 扫描所有位图块
        for bitmap_idx in 0..bitmap_blocks {
            let block_num = 1 + bitmap_idx as u32; // 位图从块#1开始
            let mut block_buf = vec![0u8; BLOCK_SIZE];
            disk.read_block(block_num, &mut block_buf);

            // 扫描该位图块中的每个字节
            for (byte_offset, &byte_val) in block_buf.iter().enumerate() {
                if byte_val == 0xFF {
                    // 该字节8个bit全满，跳过
                    continue;
                }

                // 找到第一个为0的bit
                for bit in 0..8 {
                    let mask = 1u8 << bit;
                    if (byte_val & mask) == 0 {
                        // 找到了空闲块！
                        let free_block = ((bitmap_idx * BLOCK_SIZE + byte_offset) * 8 + bit) as u32;

                        if free_block >= MAX_BLOCKS {
                            return None;
                        }

                        // 标记为已分配
                        Self::mark_allocated(disk, sb, free_block);
                        return Some(free_block);
                    }
                }
            }
        }

        None
    }

    /// 初始化位图 —— 格式化时调用
    ///
    /// 将超级块、位图区自身、inode表区域标记为"已占用"，
    /// 数据区标记为"空闲"。
    pub fn init_bitmap(disk: &Disk, sb: &SuperBlock) {
        let bitmap_blocks = sb.bitmap_blocks as usize;
        // 总位图字节数
        let total_bytes = bitmap_blocks * BLOCK_SIZE;

        // 构造位图数据
        let mut bitmap = vec![0u8; total_bytes];

        // 将数据区之前的所有块标记为"已占用"
        for block_num in 0..sb.data_start {
            let (byte_idx, mask) = Self::locate(block_num);
            if byte_idx < total_bytes {
                bitmap[byte_idx] |= mask;
            }
        }

        // 写入所有位图块
        for i in 0..bitmap_blocks {
            let block_num = 1 + i as u32;
            let start = i * BLOCK_SIZE;
            let end = start + BLOCK_SIZE;
            let mut block_buf = vec![0u8; BLOCK_SIZE];
            block_buf.copy_from_slice(&bitmap[start..end]);
            disk.write_block(block_num, &block_buf);
        }
    }
}
