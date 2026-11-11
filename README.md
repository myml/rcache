# rcache

为一块慢盘（HDD、分区或任意块设备）提供 **write-back 写缓存**，缓存数据放在快盘上的一个**普通文件**里。

- 慢盘**不做任何格式化**，不在其上写超级块（不像 bcache 那样占用盘尾）；
- 缓存就是文件系统里的一个文件，快盘**不需要分区、不需要格式化**；
- 不依赖 device-mapper、不依赖 loop、不需要编译内核模块；
- 正常停止后脏数据已全部回写，慢盘可直接拔走，在别的机器上原样使用。

实现为 [ublk](https://docs.kernel.org/block/ublk.html)（userspace block driver）后端：

```
文件系统 ──► /dev/ublkbN ──► rcache（用户态进程） ──► 缓存文件（快盘）
                                  └──────────────► origin 慢盘（读 + 回写）
```

## 设计

**按块环形日志，日志即元数据。** 没有 b 树、没有淘汰策略、没有独立的索引文件，因此代码量很小（约 1400 行，含测试）。

缓存文件布局（小端）：

```
扇区 0        header
              magic | version | block_size | origin_size | log_off | log_len
              head | tail | next_version | CRC32(前 60 字节)
扇区 1 .. EOF 环形日志区
              记录 = 64B 头（blk, len, version, crc32, flags）+ 512B 对齐的数据
```

各操作的语义：

| 操作 | 行为 |
|---|---|
| **WRITE** | 数据作为该块的新版本追加进日志，更新 `tail`，fsync header 后返回。此时 origin 上仍是旧数据 —— 这就是 write-back。 |
| **READ** | 查内存索引（启动时扫描日志重建）；命中读缓存文件，未命中读 origin。 |
| **FLUSH** | 按版本顺序把日志中所有块写回 origin → fsync → 重置日志。 |
| **DISCARD** | 透传给 origin（`BLKDISCARD`），并丢弃对应日志副本。 |
| **日志满** | 自动先做一次 FLUSH 再追加，不报错、不丢写。 |
| **崩溃 / 断电后重启** | 扫描日志重建索引，把未回写记录重放回 origin 后再服务新请求。 |
| **写 origin 失败** | 日志**完整保留**（不清空），下次仍可恢复。 |

一致性相关的保证：

- header 带 CRC，每条记录带 CRC32，坏记录被丢弃并告警，而不是静默信任；
- header 记录 origin 的字节数，容量不符时**拒绝启动**，防止把缓存错接到别的盘上；
- 非 4 KiB 对齐的 IO（`mkfs` 实际会发）用读-改-写处理，不会写坏相邻块。

## 环境要求

- Linux 5.19+，内核需开启 `CONFIG_BLK_DEV_UBLK`（多数发行版已编入；没有则 `modprobe ublk_drv`）；
- 运行进程需要 root（或 `CAP_SYS_ADMIN`）；
- 构建需要 Rust 1.85+ 与 `libclang`（`libublk-sys` 用 bindgen 生成绑定）。

## 构建

```sh
cargo build --release      # 产物：target/release/rcache
cargo test                 # 单元测试
```

## 使用

```sh
# 1. 在快盘上预分配日志文件（64 GiB）
fallocate -l 64G /mnt/nvme/rcache.journal

# 2. 启动，自动分配 /dev/ublkbN
sudo ./target/release/rcache run --origin /dev/sdb --cache /mnt/nvme/rcache.journal

# 3. 正常使用
sudo mkfs.ext4 /dev/ublkb0
sudo mount /dev/ublkb0 /mnt/data

# 4. 安全停止
sudo umount /mnt/data
sudo ./target/release/rcache stop --id 0

# 5. 此时 /dev/sdb 上的数据完整且最新，可以直接拔走
```

`rcache.sh` 是一层便捷封装（预分配日志、参数检查）：

```sh
./rcache.sh start /dev/sdb /mnt/nvme/rcache.journal 64G
./rcache.sh stop 0
./rcache.sh status 0
```

### 命令

| 命令 | 说明 |
|---|---|
| `rcache run --origin <DEV> --cache <FILE> [--id N] [--queues N] [--depth N]` | 创建设备并服务请求 |
| `rcache stop --id N` | 停止设备 → 队列退出 → 排空日志 → 进程退出 |
| `rcache status --id N` | 打印设备信息 |

### 关闭语义

- **`rcache stop`**：完整拆除，退出前把日志全部回写到 origin。
- **SIGINT / SIGTERM**：只把日志排空到 origin，**设备继续服务 IO**，已挂载的文件系统仍可正常读写。
  适合“准备关机/拔盘”时先把脏数据落地；真正拆除仍需 `rcache stop`。

## 验证结果

在 Proxmox 主机（内核 7.0.14，ublk 驱动）上，用 loop 文件作 origin、普通文件作缓存实测：

| 验证项 | 结果 |
|---|---|
| `/dev/ublkbN` 创建、容量与 origin 一致 | ✅ |
| `mkfs.ext4` / `mount` / 写 64 MiB / `umount` | ✅ |
| 重挂载后 sha256 与写入值一致 | ✅ |
| **umount 后直接挂载 origin（不经过缓存）** | ✅ sha256 一致，origin 自洽可独立使用 |
| SIGINT（排空缓存）→ 文件系统继续可读写 → umount → 校验 origin | ✅ |
| `rcache stop` → 设备消失、进程退出、日志清空 | ✅ |
| 进程崩溃后日志回放 | ✅ |

单元测试 `cargo test` 覆盖日志格式与重开、追加与扫描、CRC 损坏、满环排空、崩溃回放、
非对齐读写，以及 WRITE→READ→FLUSH 的全链路。

> 注意：上述验证针对的是**正确性**。用 loop 文件模拟慢盘无法反映真实 HDD 的性能差异，性能尚未测量。

## 已知限制

1. **只有写缓存**，不做读缓存。读命中仅限“该块当前有未回写副本”的情形。
2. **没有后台回写线程**：脏数据在 FLUSH、日志满、收到信号这三个时机落盘。
   长期只写不 flush 的负载，延迟会在日志满时集中体现。
3. **单队列、单线程、同步 IO**。能验证正确性，不是吞吐最优形态。
4. **同一块多次写**会产生多条记录（后写覆盖先写），日志占用高于理论值。
5. 运行中**硬拔缓存盘 = 丢失未回写的写**；需先 `rcache stop` 或信号排空。

## 实现备注

- ublk 的 FLUSH 请求没有数据（`nr_sectors == 0`），提交完成时**必须携带该 tag 真实的缓冲区地址**，
  否则 `COMMIT` 会带 `addr = NULL`，驱动把该 tag 卡死，后续 IO 不再送达。
- 设备声明的缓存属性会影响内核是否拆出独立 FLUSH 请求，这是排查 `fsync` 卡死时的关键点。

## 代码结构

| 文件 | 职责 |
|---|---|
| `src/log.rs` | 缓存文件日志格式、header、追加、扫描、CRC |
| `src/cache.rs` | 索引、读写分流、flush、崩溃回放、discard |
| `src/ublk_tgt.rs` | ublk 设备创建、命令分流、信号处理 |
| `src/main.rs` | 命令行与信号处理 |

## License

Apache-2.0
