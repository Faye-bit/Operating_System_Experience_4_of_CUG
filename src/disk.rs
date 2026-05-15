//! 磁盘I/O层 —— 所有磁盘读写操作都通过系统调用实现
//!
//! 【改进说明】本模块将原C代码中的 read_block / write_block 封装为Rust安全接口。
//! 所有底层I/O操作均使用 libc 系统调用（open/read/write/lseek/close/ftruncate），
//! 不依赖任何高级文件I/O库，确保在用户空间文件系统中体验系统调用级别的磁盘访问。

use libc::{c_void, O_CREAT, O_RDWR, SEEK_SET};
use std::ffi::CString;

/// 虚拟磁盘大小：10MB
pub const DISK_SIZE: u64 = 10 * 1024 * 1024;
/// 块大小：512字节（与经典磁盘扇区大小一致）
pub const BLOCK_SIZE: usize = 512;
/// 可存储数据的区域（每块最后2字节留给"下一块指针"）
pub const PAGE_SIZE: usize = BLOCK_SIZE - std::mem::size_of::<u16>();
/// 总块数
pub const MAX_BLOCKS: u32 = (DISK_SIZE as u32) / (BLOCK_SIZE as u32);

/// 磁盘句柄 —— 封装对虚拟磁盘文件的所有操作
///
/// 【系统调用说明】内部使用 libc::open / read / write / lseek / close 等系统调用。
/// 在FUSE用户空间文件系统中，这些系统调用直接作用于宿主文件系统上的磁盘映像文件，
/// 但逻辑上模拟了块设备级别的磁盘I/O。
pub struct Disk {
    fd: i32, // 文件描述符（由系统调用 open() 返回）
}

impl Disk {
    /// 打开（或创建）虚拟磁盘文件
    ///
    /// 【系统调用】libc::open(path, O_RDWR | O_CREAT, 0o666)
    pub fn open(path: &str) -> Self {
        let c_path = CString::new(path).expect("invalid path");
        // 系统调用：open() —— 打开或创建文件
        let fd = unsafe { libc::open(c_path.as_ptr(), O_RDWR | O_CREAT, 0o666) };
        if fd < 0 {
            panic!("[syscall] open disk file '{}' failed: errno={}", path, std::io::Error::last_os_error());
        }
        Disk { fd }
    }

    /// 获取磁盘文件的当前大小
    ///
    /// 【系统调用】libc::fstat(fd, &mut stat_buf)
    pub fn size(&self) -> u64 {
        let mut stat_buf: libc::stat = unsafe { std::mem::zeroed() };
        // 系统调用：fstat() —— 获取文件元数据
        let ret = unsafe { libc::fstat(self.fd, &mut stat_buf) };
        if ret < 0 {
            panic!("[syscall] fstat failed: errno={}", std::io::Error::last_os_error());
        }
        stat_buf.st_size as u64
    }

    /// 将磁盘文件扩展到指定大小（用于新磁盘初始化）
    ///
    /// 【系统调用】libc::ftruncate(fd, size)
    pub fn truncate(&self, size: u64) {
        // 系统调用：ftruncate() —— 调整文件大小
        let ret = unsafe { libc::ftruncate(self.fd, size as libc::off_t) };
        if ret < 0 {
            panic!("[syscall] ftruncate failed: errno={}", std::io::Error::last_os_error());
        }
    }

    /// 从虚拟磁盘读取一个块（512字节）
    ///
    /// 【系统调用】libc::lseek(fd, offset, SEEK_SET) + libc::read(fd, buf, BLOCK_SIZE)
    pub fn read_block(&self, block_num: u32, buf: &mut [u8]) {
        let offset = block_num as u64 * BLOCK_SIZE as u64;
        unsafe {
            // 系统调用：lseek() —— 定位文件读写位置
            if libc::lseek(self.fd, offset as libc::off_t, SEEK_SET) < 0 {
                panic!("[syscall] lseek failed: errno={}", std::io::Error::last_os_error());
            }
            // 系统调用：read() —— 从文件读取数据
            let n = libc::read(self.fd, buf.as_mut_ptr() as *mut c_void, BLOCK_SIZE);
            if n != BLOCK_SIZE as isize {
                panic!("[syscall] read block {} failed: expected {} bytes, got {}", block_num, BLOCK_SIZE, n);
            }
        }
    }

    /// 向虚拟磁盘写入一个块（512字节），并立即同步到磁盘
    ///
    /// 【系统调用】libc::lseek() + libc::write() + libc::fsync()
    pub fn write_block(&self, block_num: u32, buf: &[u8]) {
        let offset = block_num as u64 * BLOCK_SIZE as u64;
        unsafe {
            // 系统调用：lseek() —— 定位
            if libc::lseek(self.fd, offset as libc::off_t, SEEK_SET) < 0 {
                panic!("[syscall] lseek failed: errno={}", std::io::Error::last_os_error());
            }
            // 系统调用：write() —— 写入数据
            let n = libc::write(self.fd, buf.as_ptr() as *const c_void, BLOCK_SIZE);
            if n != BLOCK_SIZE as isize {
                panic!("[syscall] write block {} failed: wrote {} bytes", block_num, n);
            }
            // 系统调用：fsync() —— 确保数据写入物理介质
            libc::fsync(self.fd);
        }
    }

    /// 读取块的一部分（不要求块对齐）
    pub fn read_partial(&self, block_num: u32, offset: usize, len: usize, buf: &mut [u8]) {
        let mut block_buf = vec![0u8; BLOCK_SIZE];
        self.read_block(block_num, &mut block_buf);
        let end = (offset + len).min(BLOCK_SIZE);
        buf[..end - offset].copy_from_slice(&block_buf[offset..end]);
    }

    /// 写入块的一部分（不要求块对齐）
    pub fn write_partial(&self, block_num: u32, offset: usize, data: &[u8]) {
        let mut block_buf = vec![0u8; BLOCK_SIZE];
        self.read_block(block_num, &mut block_buf);
        let end = (offset + data.len()).min(PAGE_SIZE);
        block_buf[offset..end].copy_from_slice(&data[..end - offset]);
        self.write_block(block_num, &block_buf);
    }
}

impl Drop for Disk {
    fn drop(&mut self) {
        // 系统调用：close() —— 关闭文件描述符
        unsafe { libc::close(self.fd) };
    }
}
