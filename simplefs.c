#define FUSE_USE_VERSION 26

#include <fuse.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <fcntl.h>
#include <unistd.h>
#include <stdint.h>
#include <time.h>

#define DISK_SIZE (10 * 1024 * 1024)  // 10MB虚拟磁盘
#define BLOCK_SIZE 512                // 每块512字节
#define PAGE_SIZE (BLOCK_SIZE-sizeof(uint16_t))
#define MAX_BLOCKS (DISK_SIZE / BLOCK_SIZE)
#define FILENAME_LEN 10               // 文件名最大10字节
#define DIR_ENTRIES_PER_BLOCK (PAGE_SIZE / sizeof(struct dir_entry))
#define MAX_FILE_BLOCKS 20            // 每个文件最多20个块(约10KB)
#define next_block_no(block_data)  (*(uint16_t*)(block_data+PAGE_SIZE))

// 目录项结构
struct dir_entry {
    char name[FILENAME_LEN];  // 文件名
    uint16_t first_block;     // 文件起始块号
    uint16_t block_count;     // 文件占用的块数
    uint32_t file_size;       // 文件实际大小
    time_t mtime;             // 修改时间
};

// 超级块结构
struct super_block {
    uint16_t first_free_block;  // 第一个空闲块的块号
    uint16_t root_dir_block;    // 根目录所在块号
    uint32_t total_blocks;      // 总块数
    uint32_t free_blocks;       // 空闲块数
    char magic[16];              // 文件系统魔数
};

// 全局变量
static int disk_fd;
static const char *disk_path = "disk.img";
static struct super_block sb;

// 辅助函数声明
static void init_disk();
static void format_disk();
static void read_block(uint16_t block_num, void *buf);
static void write_block(uint16_t block_num, const void *buf);
static uint16_t allocate_block();
static void free_block(uint16_t block_num);
static int find_entry(const char *path, struct dir_entry *entry, uint16_t *dir_block);

// ====================== FUSE操作实现 ======================

static int simplefs_getattr(const char *path, struct stat *stbuf) {
    memset(stbuf, 0, sizeof(struct stat));
    
    if (strcmp(path, "/") == 0) {
        stbuf->st_mode = S_IFDIR | 0755;
        stbuf->st_nlink = 2;
        return 0;
    }
    
    struct dir_entry entry;
    if (find_entry(path, &entry, NULL)) {
        stbuf->st_mode = S_IFREG | 0444;
        stbuf->st_nlink = 1;
        stbuf->st_size = entry.file_size;
        stbuf->st_mtime = entry.mtime;
        return 0;
    }
    
    return -ENOENT;
}

static int simplefs_readdir(const char *path, void *buf, fuse_fill_dir_t filler,
                          off_t offset, struct fuse_file_info *fi) {
    (void) offset;
    (void) fi;
    
    if (strcmp(path, "/") != 0) {
        return -ENOENT;
    }
    
    filler(buf, ".", NULL, 0);
    filler(buf, "..", NULL, 0);
    
    char block_data[BLOCK_SIZE];
    uint16_t current_dir_block = sb.root_dir_block;
    while (current_dir_block != 0) {
        read_block(current_dir_block, block_data);
        struct dir_entry *dir=(struct dir_entry*)block_data;
        for (int i = 0; i < DIR_ENTRIES_PER_BLOCK; i++) {
            if (dir[i].name[0] != '\0') {
                filler(buf, dir[i].name, NULL, 0);
            }
        }
        
        current_dir_block = next_block_no(block_data);
    }
    
    return 0;
}

static int simplefs_open(const char *path, struct fuse_file_info *fi) {
    struct dir_entry entry;
    if (!find_entry(path, &entry, NULL)) {
        return -ENOENT;
    }
    
    if ((fi->flags & O_ACCMODE) != O_RDONLY) {
        return -EACCES;
    }
    
    return 0;
}

static int simplefs_read(const char *path, char *buf, size_t size, off_t offset,
                        struct fuse_file_info *fi) {
    struct dir_entry entry;
    if (!find_entry(path, &entry, NULL)) {
        return -ENOENT;
    }
    
    if (offset >= entry.file_size) {
        return 0;
    }
    
    if (offset + size > entry.file_size) {
        size = entry.file_size - offset;
    }
    
    size_t remaining = size;
    size_t copied = 0;
    uint16_t current_block = entry.first_block;
    size_t block_offset = offset % PAGE_SIZE;
    char block_data[BLOCK_SIZE];
    
    // 跳过前面的块
    for (int i = 0; i < offset / PAGE_SIZE; i++) {
        read_block(current_block, block_data);
        current_block = next_block_no(block_data);
    }
    
    while (remaining > 0 && current_block != 0) {
        read_block(current_block, block_data);
        
        size_t to_copy = PAGE_SIZE - block_offset;
        if (to_copy > remaining) {
            to_copy = remaining;
        }
        
        memcpy(buf + copied, block_data + block_offset, to_copy);
        
        copied += to_copy;
        remaining -= to_copy;
        current_block = next_block_no(block_data);
        block_offset = 0;
    }
    
    return copied;
}

static int simplefs_create(const char *path, mode_t mode, struct fuse_file_info *fi) {
    (void) mode;
    
    if (strlen(strrchr(path, '/') + 1) > FILENAME_LEN) {
        return -ENAMETOOLONG;
    }
    
    // 检查文件是否已存在
    struct dir_entry entry;
    if (find_entry(path, &entry, NULL)) {
        return -EEXIST;
    }
    
    // 在目录中找空位
    uint16_t current_dir_block = sb.root_dir_block;
    char block_data[BLOCK_SIZE];
    struct dir_entry *dir;
    int found = 0;
    int entry_index = -1;
    
    while (current_dir_block != 0 && !found) {
        read_block(current_dir_block, block_data);
        dir=(struct dir_entry *)block_data;
        
        for (int i = 0; i < DIR_ENTRIES_PER_BLOCK; i++) {
            if (dir[i].name[0] == '\0') {
                entry_index = i;
                found = 1;
                break;
            }
        }
        
        if (!found) {
            if (next_block_no(block_data) == 0) {
                // 分配新目录块
                uint16_t new_dir_block = allocate_block();
                if (new_dir_block == 0) {
                    return -ENOSPC;
                }
                
                next_block_no(block_data) = new_dir_block;
                write_block(current_dir_block, block_data);
                
                current_dir_block = new_dir_block;
                memset(block_data, 0, BLOCK_SIZE);
                dir=(struct dir_entry *)block_data;
                entry_index = 0;
                found = 1;
            } else {
                current_dir_block = next_block_no(block_data);
            }
        }
    }
    
    if (!found) {
        return -ENOSPC;
    }
    
    // 创建新文件
    strncpy(dir[entry_index].name, strrchr(path, '/') + 1, FILENAME_LEN);
    dir[entry_index].first_block = 0;  // 初始无数据块
    dir[entry_index].block_count = 0;
    dir[entry_index].file_size = 0;
    dir[entry_index].mtime = time(NULL);
    
    write_block(current_dir_block, block_data);
    return 0;
}

static int simplefs_write(const char *path, const char *buf, size_t size,
                         off_t offset, struct fuse_file_info *fi) {
    struct dir_entry entry;
    uint16_t dir_block;
    if (!find_entry(path, &entry, &dir_block)) {
        return -ENOENT;
    }
    
    // 计算需要的块数
    size_t required_blocks = (offset + size + PAGE_SIZE - 1) / PAGE_SIZE;
    if (required_blocks > MAX_FILE_BLOCKS) {
        return -EFBIG;
    }
    
    char block_data[BLOCK_SIZE];
    // 分配或释放块以满足需求
    if (required_blocks > entry.block_count) {
        // 需要分配更多块
        uint16_t last_block = entry.first_block;
        if (last_block != 0) {
            read_block(last_block, block_data);
            while (next_block_no(block_data) != 0) {
                last_block = next_block_no(block_data);
            }
        }
        
        for (int i = entry.block_count; i < required_blocks; i++) {
            uint16_t new_block = allocate_block();
            if (new_block == 0) {
                // 空间不足，回滚
                return -ENOSPC;
            }
            
            if (entry.first_block == 0) {
                entry.first_block = new_block;
            } else {
                next_block_no(block_data)=new_block;
                write_block(last_block, block_data);
            }
            
            last_block = new_block;
            memset(block_data, 0, BLOCK_SIZE);
            entry.block_count++;
        }
    } else if (required_blocks < entry.block_count) {
        // 需要释放多余的块
        uint16_t block_to_keep = entry.first_block;
        for (int i = 1; i < required_blocks; i++) {
            read_block(block_to_keep, block_data);
            block_to_keep = next_block_no(block_data);
        }
        
        read_block(block_to_keep, block_data);
        uint16_t block_to_free = next_block_no(block_data);
        next_block_no(block_data)=0;// 终止链
        write_block(block_to_keep, block_data);  
        
        while (block_to_free != 0) {
            read_block(block_to_free, block_data);
            uint16_t next = next_block_no(block_data);
            free_block(block_to_free);
            block_to_free = next;
            entry.block_count--;
        }
    }
    
    // 写入数据
    size_t remaining = size;
    uint16_t current_block = entry.first_block;
    size_t block_offset = offset % PAGE_SIZE;
    
    // 跳过前面的块
    for (int i = 0; i < offset / PAGE_SIZE; i++) {
        read_block(current_block, block_data);
        current_block = next_block_no(block_data);
    }
    
    while (remaining > 0 && current_block != 0) {
        read_block(current_block, block_data);
        
        size_t to_write = PAGE_SIZE - block_offset;
        if (to_write > remaining) {
            to_write = remaining;
        }
        
        memcpy(block_data + block_offset, buf + (size - remaining), to_write);
        write_block(current_block, block_data);
        
        remaining -= to_write;
        current_block = next_block_no(block_data);
        block_offset = 0;
    }
    
    // 更新目录项
    entry.file_size = offset + size > entry.file_size ? offset + size : entry.file_size;
    entry.mtime = time(NULL);
    
    struct dir_entry *dir;
    read_block(dir_block, block_data);
    dir=(struct dir_entry*)block_data;
    for (int i = 0; i < DIR_ENTRIES_PER_BLOCK; i++) {
        if (strcmp(dir[i].name, entry.name) == 0) {
            dir[i] = entry;
            break;
        }
    }
    write_block(dir_block, block_data);
    
    return size;
}

static int simplefs_unlink(const char *path) {
    struct dir_entry entry;
    uint16_t dir_block;
    if (!find_entry(path, &entry, &dir_block)) {
        return -ENOENT;
    }
    
    // 释放文件占用的所有块
    char block_data[BLOCK_SIZE];
    uint16_t current_block = entry.first_block;
    while (current_block != 0) {
    	read_block(current_block, block_data);
        uint16_t next_block = next_block_no(block_data);
        free_block(current_block);
        current_block = next_block;
    }
    
    // 从目录中删除条目
    struct dir_entry *dir;
    read_block(dir_block, block_data);
    dir=(struct dir_entry*)block_data;
    for (int i = 0; i < DIR_ENTRIES_PER_BLOCK; i++) {
        if (strcmp(dir[i].name, entry.name) == 0) {
            memset(&dir[i], 0, sizeof(struct dir_entry));
            break;
        }
    }
    write_block(dir_block, block_data);
    
    return 0;
}

static struct fuse_operations simplefs_oper = {
    .getattr = simplefs_getattr,
    .readdir = simplefs_readdir,
    .open = simplefs_open,
    .read = simplefs_read,
    .create = simplefs_create,
    .write = simplefs_write,
    .unlink = simplefs_unlink,
};

// ====================== 辅助函数实现 ======================

static void init_disk() {
    disk_fd = open(disk_path, O_RDWR | O_CREAT, 0666);
    if (disk_fd < 0) {
        perror("open disk file failed");
        exit(1);
    }
    
    // 检查磁盘文件大小，如果不存在则创建
    struct stat st;
    if (fstat(disk_fd, &st) < 0) {
        perror("fstat failed");
        exit(1);
    }
    
    if (st.st_size == 0) {
        // 新磁盘，需要格式化
        if (ftruncate(disk_fd, DISK_SIZE) < 0) {
            perror("ftruncate failed");
            exit(1);
        }
        format_disk();
    } else if (st.st_size != DISK_SIZE) {
        fprintf(stderr, "Disk size mismatch\n");
        exit(1);
    }
    
    // 读取超级块
    char block_data[BLOCK_SIZE]; 
    read_block(0, block_data);
    memcpy(&sb,block_data,sizeof(sb));
    if (strcmp(sb.magic, "SIMPLEFS") != 0) {
        fprintf(stderr, "Invalid filesystem magic\n");
        exit(1);
    }
}

static void format_disk() {
    // 初始化超级块
    memset(&sb, 0, sizeof(sb));
    strcpy(sb.magic, "SIMPLEFS");
    sb.total_blocks = MAX_BLOCKS;
    sb.free_blocks = MAX_BLOCKS - 1;  // 减去超级块本身
    
    // 设置空闲块链
    sb.first_free_block = 1;  // 块0是超级块
    
    // 初始化空闲块链
     char block_data[BLOCK_SIZE]; 
     memset(block_data, 0, BLOCK_SIZE);
    uint16_t next_free = 0;
    for (uint16_t i = 1; i < MAX_BLOCKS; i++) {
        next_free = (i == MAX_BLOCKS - 1) ? 0 : i + 1;
        next_block_no(block_data)=next_free;
        write_block(i, block_data);
    }
    
    // 创建根目录
    sb.root_dir_block = allocate_block();
    //memset(block_data, 0, BLOCK_SIZE);
    write_block(sb.root_dir_block, block_data);
    
    // 写入超级块
    memcpy(block_data,&sb,sizeof(sb));
    write_block(0, block_data);
}

static void read_block(uint16_t block_num, void *buf) {
    lseek(disk_fd, block_num * BLOCK_SIZE, SEEK_SET);
    if (read(disk_fd, buf, BLOCK_SIZE) != BLOCK_SIZE) {
        perror("read block failed");
        exit(1);
    }
}

static void write_block(uint16_t block_num, const void *buf) {
    lseek(disk_fd, block_num * BLOCK_SIZE, SEEK_SET);
    if (write(disk_fd, buf, BLOCK_SIZE) != BLOCK_SIZE) {
        perror("write block failed");
        exit(1);
    }
}

static uint16_t allocate_block() {
    if (sb.free_blocks == 0) {
        return 0;  // 没有空闲块
    }
    
    uint16_t allocated_block = sb.first_free_block;
    uint16_t next_free;
    char block_data[BLOCK_SIZE]; 
    read_block(allocated_block, block_data);
    next_free=next_block_no(block_data);
    
    sb.first_free_block = next_free;
    sb.free_blocks--;
    memset(block_data, 0, BLOCK_SIZE);
    memcpy(block_data,&sb,sizeof(sb));
    write_block(0, block_data);
    
    return allocated_block;
}

static void free_block(uint16_t block_num) {
    uint16_t old_first = sb.first_free_block;
    sb.first_free_block = block_num;
    
    char block_data[BLOCK_SIZE]; 
    memset(block_data, 0, BLOCK_SIZE);
    next_block_no(block_data)=old_first;
    write_block(block_num, block_data);
    
    sb.free_blocks++;
    memset(block_data, 0, BLOCK_SIZE);
    memcpy(block_data,&sb,sizeof(sb));
    write_block(0, block_data);
}

static int find_entry(const char *path, struct dir_entry *entry, uint16_t *dir_block) {
    if (strcmp(path, "/") == 0) {
        return 0;  // 根目录
    }
    
    char *name = strrchr(path, '/') + 1;
    
    uint16_t current_dir_block = sb.root_dir_block;
    while (current_dir_block != 0) {
        char block_data[BLOCK_SIZE];
        read_block(current_dir_block, block_data);
        struct dir_entry *dir=(struct dir_entry *)block_data;
        
        for (int i = 0; i < DIR_ENTRIES_PER_BLOCK; i++) {
            if (strcmp(dir[i].name, name) == 0) {
                *entry = dir[i];
                if (dir_block) *dir_block = current_dir_block;
                return 1;
            }
        }
        
        current_dir_block = next_block_no(block_data);
    }
    
    return 0;  // 没找到
}

int main(int argc, char *argv[]) {
    init_disk();
    return fuse_main(argc, argv, &simplefs_oper, NULL);
}

