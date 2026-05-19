# SimpleFS —— 基于FUSE的简单文件系统（Rust实现）

## 实验概述

本实验基于FUSE（Filesystem in Userspace）框架，使用Rust语言实现了一个用户空间文件系统。在原有C语言简单文件系统的基础上，对**空闲空间管理**、**文件目录结构**和**共享与安全**三个方面进行了改进。

## 实验流程与设计思路

### Step 1：分析原有C代码

原有`simplefs.c`实现了一个基础的文件系统，包含以下组件：
- **超级块（SuperBlock）**：存储文件系统全局信息（魔数、总块数、空闲块数等）
- **目录项（dir_entry）**：扁平结构，文件名直接关联到数据块链
- **链式空闲块管理**：每个空闲块存储下一个空闲块的块号
- **FUSE操作**：getattr、readdir、open、read、create、write、unlink

原代码的主要局限：
1. 空闲块使用链表管理，分配和释放需要遍历链表
2. 只支持根目录（`/`），无法创建子目录
3. 没有任何权限控制，所有文件默认只读
4. 文件元数据嵌入在目录项中，耦合度高

### Step 2：设计改进方案

针对上述局限，设计了三个改进方向：

#### 改进一：位图空闲空间管理（Bitmap Free Space Management）

**原理**：使用位图（bitmap）记录每个块的使用状态，每个块对应1个bit（0=空闲，1=已占用）。

**优势**：
- 分配/释放是O(1)的位操作（原链表需O(n)遍历）
- 可O(1)判断任意块的分配状态
- 位图集中存放在磁盘前部，不污染数据块
- 这是ext2/ext4等现代文件系统的标准做法

**实现要点**：
- 位图存储在超级块之后的连续块中（块#1~#5）
- 10MB磁盘有20480个块，需要 20480 bits = 2560 bytes ≈ 5 blocks
- `allocate_block()`：扫描位图找到第一个空闲bit，将其置1
- `mark_free()`：将对应bit清零，更新超级块中的空闲块计数

#### 改进二：层次化目录结构（Hierarchical Directories）

**原理**：将"目录"视为一种特殊文件，其"文件内容"是一系列目录项（DirEntry）的列表。每个目录项记录"文件名→inode号"的映射。

**优势**：
- 支持任意深度的目录嵌套（`/home/user/docs/file.txt`）
- 目录和文件通过统一的inode机制管理
- 目录项与inode分离，支持硬链接

**实现要点**：
- 引入inode（索引节点）结构，独立管理文件元数据和块指针
- 目录项结构：`(inode号: u32, 条目类型: u8, 文件名长度: u8, 文件名: [u8; 28])`
- 新增`mkdir`操作：创建目录inode + 初始化`.`和`..`条目
- 新增`rmdir`操作：检查目录为空后释放
- 路径解析由FUSE框架的`lookup`调用逐级完成

#### 改进三：Unix权限系统（Unix Permission System）

**原理**：在inode中存储`uid`、`gid`和`mode`字段，实现标准的Unix rwx权限模型。

**优势**：
- 支持文件所有者、用户组和其他用户的读/写/执行权限控制
- 与宿主Linux系统的权限模型一致
- 支持`chmod`动态修改权限

**实现要点**：
- inode中新增`mode: u16`字段（低9位为rwx权限，高4位为文件类型）
- `open()`操作中进行权限检查（`check_permission()`）
- 新增`setattr()`操作，支持chmod修改权限
- 默认权限：文件0644（rw-r--r--），目录0755（rwxr-xr-x）

### Step 3：Rust代码架构

```
src/
├── main.rs          # 入口：解析参数，挂载文件系统
├── disk.rs          # 磁盘I/O层（通过libc系统调用）
├── superblock.rs    # 超级块管理（元数据读写）
├── bitmap.rs        # 位图空闲空间管理（★改进一）
├── inode.rs         # inode结构与管理（★改进二、三的基础）
├── directory.rs     # 目录操作（★改进二）
└── fs.rs            # FUSE Filesystem trait实现
```

**关于系统调用**：所有磁盘操作均通过`libc` crate直接调用Linux系统调用（open/read/write/lseek/close/ftruncate/fsync/fstat），符合实验要求。

### Step 4：磁盘布局设计

```
┌──────────┬──────────┬──────────┬───────────────────────┐
│  超级块   │  位图区   │  inode表  │      数据块区          │
│  (块0)    │ (块1~5)  │ (块6~30) │    (块31~20479)       │
│  512B     │  2560B   │  12800B  │      ≈10MB            │
└──────────┴──────────┴──────────┴───────────────────────┘
```

超级块格式（44字节）：
  - magic: [u8; 16]    = "SIMPLEFS-RUST\0\0\0"
  - total_blocks: u32  = 20480
  - free_blocks: u32   （空闲块计数）
  - bitmap_blocks: u32 = 5
  - inode_table_start: u32 = 6
  - inode_table_blocks: u32 = 25
  - data_start: u32    = 31
  - root_inode: u32    = 1（FUSE协议规定必须为1）

Inode格式（84字节/个）：
  - mode: u16             （文件类型 + 权限位）
  - uid: u16              （所有者用户ID）
  - gid: u16              （所有者组ID）
  - size: u32             （文件大小，字节）
  - nlink: u16            （硬链接计数，0表示空闲inode）
  - block_count: u16      （已分配数据块数）
  - mtime: i64            （最后修改时间）
  - atime: i64            （最后访问时间）
  - ctime: i64            （创建时间）
  - direct_blocks: [u32; 10] （10个直接块指针，每块510B ≈ 5KB）
  - indirect_block: u32   （1个一级间接块指针 ≈ 127KB）

目录项格式（34字节/个，每块可存15个）：
  - inode: u32            （指向的inode号）
  - entry_type: u8        （0=空闲, 1=普通文件, 2=目录）
  - name_len: u8          （文件名实际长度）
  - name: [u8; 28]        （文件名，UTF-8编码）

## 编译与运行

### 前置条件（Ubuntu系统）

```bash
# 1. 安装FUSE开发库
sudo apt update
sudo apt install libfuse-dev pkg-config

# 2. 安装Rust工具链（如未安装）
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env

# 3. 加载FUSE内核模块（如未加载）
sudo modprobe fuse
```

### 编译

```bash
cd Operating_System_Experience_4_of_CUG
cargo build --release
```

编译产物位于 `target/release/simplefs`。

### 运行与测试

#### 准备工作：挂载文件系统

```bash
# 1. 创建挂载点
mkdir -p /tmp/mnt

# 2. 挂载文件系统（前台运行模式）
./target/release/simplefs -f /tmp/mnt
```

程序将输出类似以下内容，然后进入事件循环等待请求：
```
[SimpleFS] 正在挂载文件系统到 /tmp/mnt ...
[SimpleFS] 磁盘映像: disk.img (10.0MB)
[SimpleFS] 块大小: 512B, 总块数: 20480
[SimpleFS] 空闲块数: 20448
[SimpleFS] 改进特性:
           - 位图空闲空间管理 (Bitmap)
           - 层次化目录结构
           - Unix权限系统
[SimpleFS] 文件系统已就绪，在另一个终端中操作 /tmp/mnt 即可测试
[SimpleFS] 按 Ctrl+C 退出...
```

#### 测试操作（在另一个终端中执行）

```bash
# ===== 基本文件操作 =====
echo "Hello, SimpleFS!" > /tmp/mnt/test.txt
cat /tmp/mnt/test.txt
ls -la /tmp/mnt/

# ===== 目录操作（★改进：层次目录） =====
mkdir /tmp/mnt/subdir
echo "Nested file" > /tmp/mnt/subdir/nested.txt
ls -la /tmp/mnt/subdir/
cat /tmp/mnt/subdir/nested.txt

# ===== 权限测试（★改进：Unix权限） =====
chmod 600 /tmp/mnt/test.txt
ls -la /tmp/mnt/test.txt

# ===== 文件删除 =====
rm /tmp/mnt/test.txt
rm /tmp/mnt/subdir/nested.txt
rmdir /tmp/mnt/subdir
ls -la /tmp/mnt/
```

#### 卸载

```bash
# 方法1：在运行 simplefs 的终端按 Ctrl+C
# 方法2：使用 fusermount 命令
fusermount -u /tmp/mnt
```

> **注意**：首次运行时程序会在当前目录自动创建 `disk.img`（10MB 虚拟磁盘映像）。后续运行会加载已有的 `disk.img`，文件数据会持久保留。

## 常见问题排查

### 再次运行前清理旧状态

如果上一次运行异常退出（如崩溃、强制终止），挂载点和磁盘映像可能处于不一致状态。**建议每次重新测试前执行以下清理命令**：

```bash
# 1. 卸载可能残留的挂载
fusermount -u /tmp/mnt 2>/dev/null
sudo umount /tmp/mnt 2>/dev/null

# 2. 删除旧的磁盘映像（重要！格式变化时必须删除）
rm -f disk.img

# 3. 重新创建挂载点
mkdir -p /tmp/mnt

# 4. 重新编译并运行
cargo build --release
./target/release/simplefs -f /tmp/mnt
```

> **特别提醒**：如果拉取了代码更新（特别是超级块或inode格式有变化），**必须删除旧的 `disk.img`**，否则新旧格式不兼容会导致挂载失败或数据读取错误。

### 挂载失败：`fusermount: option allow_other only allowed if...`

本程序默认不启用 `allow_other` 选项，不会触发此错误。如果你确实需要此选项，编辑 `/etc/fuse.conf`，取消 `user_allow_other` 的注释。

### 挂载失败：`Unspecified Error`

通常由以下原因导致：
- FUSE 内核模块未加载：`sudo modprobe fuse`
- 挂载点被占用：`fusermount -u /tmp/mnt` 后再试
- 磁盘映像损坏：`rm -f disk.img` 后重试
- libfuse 未安装：`sudo apt install libfuse-dev`

### `ls` 报 Input/output error (EIO)

说明 `readdir` 返回了无效的 inode 号，内核无法获取条目属性。通常是因为 `disk.img` 格式不兼容（如旧版本创建的映像）。解决方法：删除 `disk.img` 重新挂载。

## 测试要点说明

| 测试项 | 对应改进 | 预期结果 |
|--------|----------|----------|
| `echo > file` + `cat file` | 基本读写 | 数据正确写入和读取 |
| `ls -la /` | 目录列表 | 列出文件及其属性 |
| `mkdir` 创建子目录 | 改进二：层次目录 | 可创建多级嵌套子目录 |
| 子目录中读写文件 | 改进二：层次目录 | 文件正确存储在嵌套路径中 |
| `chmod` 修改权限 | 改进三：权限系统 | `ls -la` 显示更新后的权限 |
| 创建和删除大量文件 | 改进一：位图管理 | 空间正确分配和回收 |
| 卸载后重新挂载 | 数据持久性 | 之前创建的文件仍然存在 |

## 关键代码说明

### 1. 位图分配块（`src/bitmap.rs`）

```rust
// 扫描位图找到第一个空闲块，通过位操作实现O(1)状态判断
for bit in 0..8 {
    let mask = 1u8 << bit;
    if (byte_val & mask) == 0 {
        // 找到空闲块，标记为已分配
        Self::mark_allocated(disk, sb, free_block);
        return Some(free_block);
    }
}
```

### 2. 层次目录创建（`src/fs.rs`）

```rust
// 目录是特殊文件，其"内容"为DirEntry列表
// . 指向自身，.. 指向父目录
let dot = DirEntry::new(new_inode_no, ".", true);
let dotdot = DirEntry::new(parent as u32, "..", true);
```

### 3. FUSE根inode约定（`src/fs.rs`）

```rust
// FUSE 协议规定根目录必须为 inode 1（FUSE_ROOT_ID）
// 内核所有对根目录的操作都使用 parent=1
// 如果使用其他 inode 号，会导致 getattr(1) 读到未初始化的数据
let root_inode_no = 1u32;
```

### 4. 权限检查（`src/inode.rs`）

```rust
// 在open操作中调用，验证用户是否有权访问文件
pub fn check_permission(&self, want_write: bool, _uid: u32, _gid: u32) -> bool {
    let perm = self.mode & 0o777;
    if want_write { (perm & 0o222) != 0 }
    else          { (perm & 0o444) != 0 }
}
```

### 5. 系统调用示例（`src/disk.rs`）

```rust
// 所有磁盘操作均使用libc系统调用
pub fn read_block(&self, block_num: u32, buf: &mut [u8]) {
    unsafe {
        libc::lseek(self.fd, offset as libc::off_t, SEEK_SET); // 定位
        libc::read(self.fd, buf.as_mut_ptr() as *mut c_void, BLOCK_SIZE); // 读取
    }
}
```

### 6. 未初始化inode保护（`src/fs.rs`）

```rust
// nlink == 0 表示该 inode 未被分配，返回 ENOENT 而非垃圾数据
// 防止访问到从未初始化的 inode 导致内核报 EIO
fn getattr(&mut self, _req: &Request, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
    let inode = read_inode(&self.disk, &self.sb, ino as u32);
    if inode.nlink == 0 {
        reply.error(ENOENT);
        return;
    }
    // ...
}
```

## 依赖项

- **Rust** ≥ 1.70
- **libfuse** ≥ 2.9（Ubuntu: `apt install libfuse-dev`）
- Rust crate依赖（Cargo自动下载）：
  - `fuser` — Rust FUSE绑定（仅Linux平台）
  - `libc` — 系统调用接口

## 与平台兼容性

- **Linux (Ubuntu)**：完整支持，FUSE 挂载和所有文件操作均可正常使用
- **macOS**：仅支持编译验证（`cargo check`），FUSE 挂载功能不可用。`fuser` crate 依赖 libfuse/macFUSE，且 macOS 无原生 FUSE 支持
- 编译时 `fuser` 依赖仅在 Linux 目标平台引入，macOS 上不会尝试链接 FUSE 库

## 与原C代码的对比总结

| 特性 | 原C实现 (simplefs.c) | Rust改进版 (本实现) |
|------|---------------------|-------------------|
| 空闲空间管理 | 链式空闲块（O(n)遍历） | 位图（O(1) bit操作） |
| 目录结构 | 仅根目录 `/` | 多级层次目录 |
| 权限控制 | 无（所有文件只读） | Unix rwx权限 + chmod |
| 文件元数据 | 嵌入目录项 | 独立inode结构 |
| 最大文件大小 | ~10KB（20个直接块×510B） | ~132KB（10直接块+1间接块） |
| 硬链接支持 | 不支持 | 支持（nlink字段） |
| 语言 | C | Rust（内存安全） |
| 系统调用 | 直接使用C标准库 | 通过libc crate调用 |

## 开发调试记录

本项目从原C代码出发，经历了多次迭代修复才达到稳定状态。以下记录了遇到的主要坑点，供参考：

1. **`is_valid()` 魔数长度错误**：`b"SIMPLEFS-RUST"` 是13字节，但比较时用了 `..12`，导致魔数校验始终失败
2. **`fuser` trait 签名不匹配**：不同版本 `fuser` 的方法签名不同（`getattr` 的 `fh` 参数、`create`/`mkdir` 的 `mode` 类型、`setattr` 的时间戳类型）
3. **FUSE根inode必须为1**：FUSE协议硬编码根目录为inode 1，使用其他inode号会导致内核操作在错误的inode上
4. **`readdir` 中 `..` 的inode错误**：原代码将 `..` 硬编码为inode 1，应与 `.` 一样从目录数据块中读取正确值
5. **未初始化inode的保护**：`getattr` 需检查 `nlink==0` 来识别空闲inode，否则返回垃圾数据导致内核报EIO

## 许可

本实验代码仅供学习交流使用。
