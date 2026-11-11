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
| **后台回写** | 空闲、水位或写者被阻塞时，增量地把日志前缀写回 origin。 |
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
| `rcache run --origin <DEV> --cache <FILE> [--id N] [--queues N] [--depth N] [--wb-high PCT] [--wb-low PCT] [--wb-step BLOCKS] [--wb-idle SECS]` | 创建设备并服务请求 |
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

## 后台回写

`src/ublk_tgt.rs` 里有一个回写线程，按 `WriteBack` 配置工作（命令行可调）：

| 触发条件 | 行为 |
|---|---|
| 日志使用率 ≥ `--wb-high`（默认 50%） | 持续回写，直到降到 `--wb-low`（默认 20%） |
| 日志在一段时间内没变化 ≥ `--wb-idle`（默认 5 秒） | 空闲回写，一直排到低水位 |
| 写请求遇到 ≥ `--wb-max`（默认 90%） | 阻塞等待回写腾出空间，避免整环排空 |

每次回写最多写 `--wb-step` 个块（默认 1024 = 4 MiB），对 HDD 友好，不会独占磁盘。

单次回写的处理顺序刻意对机械盘做过优化：

1. 只回写日志**前端的一段**，尾部保留，写请求继续追加；
2. 同一块在这段里出现多次时**只写最新版本**，早期版本直接跳过（免写放大）；
3. 幸存记录**按块号排序**后写，把随机写变成大段顺序写。

回写成功后回收这段日志（`head` 前移），失败则保留全部记录以便恢复。

## 已知限制

1. **只有写缓存**，不做读缓存。读命中仅限“该块当前有未回写副本”的情形。
2. **单队列、单线程、同步 IO**。每块写日志后都要 `fsync` header，这是当前的性能瓶颈
   （见下节实测）。
3. **同一块多次写**会产生多条记录，日志占用高于理论值；回写时会合并，但环内依然占空间。
4. 运行中**硬拔缓存盘 = 丢失未回写的写**；需先 `rcache stop` 或信号排空。

## 性能说明

测试机上 loop 文件同时充当 origin 和缓存（都在 NVMe 上）：

| 配置 | 4 KiB 随机写 | 128 KiB 顺序写 |
|---|---|---|
| 直写 origin（loop 文件） | ~965 MiB/s | — |
| 经 rcache | ~133 MiB/s | ~171 MiB/s |

**这个对比不代表真实收益**：origin 是 NVMe 上的文件，比 HDD 快两三个数量级，
rcache 的同步路径在这里是纯开销。真实 HDD 随机写通常只有 0.5–2 MiB/s，
所以按上表数据，缓存对 HDD 的随机写应有 **60× 以上**的改善——但**尚未在真实 HDD 上测过**。

### 已经做过的优化：批量提交 header

最初的实现每追加一条记录就 `fsync` 一次 header。现在改成：

- 记录写入和 header 更新先落到页缓存，**最后的 fsync 每 64 次追加才做一次**
  （`BATCH_APPENDS`）；
- 批量写一条命令时，即使跨多个块也只写**一次** header；
- 在真正需要承诺持久化的地方强制落盘：FLUSH、每次回写前、关闭时。

崩溃/断电最多丢失最近 `BATCH_APPENDS` 条追加（数据仍在页缓存里时不算丢失）。
实测 `kill -9` 后重启，journal 里的记录完整回放到 origin，数据正确。

### 仍然可优化

- 每块仍有一次 `write_all_at` 系统调用，且所有队列线程共用一把全局锁；
  改成异步 IO（io_uring 批量提交）可以进一步提升吞吐。
- 但按上面的估算，对 HDD 场景当前性能已远超需要，这些优化的收益有限。

## 实现备注

排查过程中遇到、并已在代码里处理的问题（都只在真实设备上才暴露）：

- **命令可能大于 tag 缓冲区**：ublk 的每个 tag 有固定大小的缓冲区，一条块层请求可能比它大。
  必须按缓冲区窗口就地处理整条命令；截断会静默丢数据，而用副本处理会写错内容。
- **环形日志的记录不能跨越环尾**：追加时若剩余空间放不下整条记录，要跳到环首写；
  否则 `scan()` 会在跨尾记录处停下，之后的记录全部不可见，索引随之丢失。
- **回收日志必须按记录计数推进**：用调用方算出的字节偏移推进 head，会在环绕回时
  算错 `used()`，把环“算成满的”，随后每次写都触发整环排空。
- **设备创建可能因残留成功前的 id 失败**（`EOPNOTSUPP`）：
  失败时进程会给出提示，但测试脚本务必确认 `/dev/ublkbN` 存在后再继续，
  否则会写到一个已失效的设备节点上，产生难以排查的假故障。

## 实现备注（续）

- ublk 的 FLUSH 请求没有数据（`nr_sectors == 0`），提交完成时**必须携带该 tag 真实的缓冲区地址**，
  否则 `COMMIT` 会带 `addr = NULL`，驱动把该 tag 卡死，后续 IO 不再送达。
- 设备声明的缓存属性会影响内核是否拆出独立 FLUSH 请求，这是排查 `fsync` 卡死时的关键点。

## 代码结构

| 文件 | 职责 |
|---|---|
| `src/log.rs` | 缓存文件日志格式、header、追加、扫描、回收、CRC |
| `src/cache.rs` | 索引、读写分流、增量回写、flush、崩溃回放、discard |
| `src/ublk_tgt.rs` | ublk 设备、命令分流、后台回写线程、信号处理 |
| `src/main.rs` | 命令行与信号处理 |

## License

Apache-2.0

## 统计与可观测性

### 命令行

```sh
rcache status --id 0                    # 人读的报告
rcache status --id 0 --format json      # 给脚本/监控
rcache status --id 0 --watch 2          # 每 2 秒刷新
rcache metrics                          # 所有设备的 Prometheus 文本
rcache metrics --format json
```

`status` 报告的内容：

```
device            /dev/ublkb0
  id              0
  origin          /dev/sdb (1.0 TiB)
  cache           /var/lib/rcache/sdb.journal (64.0 GiB)
  journal in use  2.1 GiB (3.3%)  live blocks 51234
  uptime          3600s

io
  reads           413 blocks  (hit 1 / miss 412)  hit ratio 0.2%
  writes          39163 blocks
  cache written   153.0 MiB
  total ops       39583
  flushes         7
  discards        0
  failures        0 (write-back 0)

write-back
  blocks written  27126  (3 superseded, 106.0 MiB bytes)
  records drained 27129
  pressure        2 space waits, 0 journal-full drains
  policy          high 50% / low 20% / max 90%, step 64 blocks, idle 5s

latency (us)      p50      p90      p99      max     mean
read                  10       25      100      115        7
write                 10       10       10   112833        6
flush                 25   100000   100000    66545    12783
write-back         10000    10000   100000    53873     8598

health            ok
```

### HTTP 端点

`rcache run --metrics-listen 127.0.0.1:9101` 会启动一个极简 HTTP 服务：

| 路径 | 内容 |
|---|---|
| `/metrics` | Prometheus 文本格式，所有在运行的设备 |
| `/status` | JSON，所有在运行的设备 |
| `/healthz` | `200 ok` / `503 degraded`（有 IO 或回写错误时降级） |

注意它是**读取共享状态文件**，不碰缓存锁，所以抓取不会影响 IO。

### 统计怎么共享的

`rcache run` 把计数器写进 `/run/rcache/<id>.stats`（mmap 文件），
旁边 `<id>.info` 放设备路径。`rcache status` 直接读这个文件，
所以不需要 IPC，也不会因为设备忙而卡住。

文件里记录了**发布者 pid**：读的时候会检查 `/proc/<pid>` 是否存在，
所以崩溃留下的旧文件不会被当成在运行的设备。

## 配置与部署

### 配置文件

```sh
rcache run --config /etc/rcache/0.toml
```

```toml
origin = "/dev/sdb"
cache  = "/var/lib/rcache/sdb.journal"
id     = 0

[ublk]
queues = 1
depth  = 64

[write_back]
high_pct    = 50
low_pct     = 20
max_pct     = 90
step_blocks = 64
idle_secs   = 5

[metrics]
listen = "127.0.0.1:9101"
```

命令行参数会覆盖配置文件。未知的键会**报错**而不是静默忽略
（防止 `hihg_pct` 这种拼写错误悄悄用默认值）。

### systemd 与安装

```sh
make            # 构建
make install    # 装二进制、systemd unit、示例配置、modules-load.d
```

之后：

```sh
vi /etc/rcache/0.toml
systemctl enable --now rcache@0
rcache status --id 0
```

unit 名为 `rcache@<name>`，读取 `/etc/rcache/<name>.toml`，因此可以并行跑多个设备。
unit 里带了基本的加固（`ProtectSystem=strict`、`NoNewPrivileges`、只放行 ublk 设备）。

## 基准测试

```sh
sudo bench/bench.sh --size 1G --journal 256M --runtime 20 --json out.json
```

脚本在一台**慢盘**（可指定，默认 `/mnt/mergerfs`，即 NFS/HDD 存储）上创建虚拟磁盘做 origin，
在快盘上放日志，然后对比"直写 origin"与"经 rcache"。它自己管理 loop 设备、
挂载点和 rcache 进程，结束后清理。

### 实测结果

在 `192.168.0.220` 上测（origin = NFS 上的 loop 文件，日志在本地 NVMe，
`mkfs.ext4` + 4 KiB 随机写 20 秒）：

| 配置 | IOPS | 带宽 | p99 |
|---|---|---|---|
| 直写 origin（loop + ext4，无 rcache） | ~13000 | ~51 MiB/s | 189 µs |
| 经 rcache | ~4000 | ~16 MiB/s | 3.8 ms |

**在这台机器上 rcache 仍是负优化（约 0.3×）**，但原因已经查清，见下。

### 排查过程中发现的一个严重 bug

最初测得只有 **113 IOPS**，写延迟 p50 高达 **500 ms**。用 `strace -c` 定位到
10 秒内有 8389 次 `fdatasync`，与写块数几乎 1:1。

根因在 `Log::sync_header()`：批量提交改造时，它被改成"写 header + fsync"，
但**忘了把 `appended_since_sync` 计数器清零**。于是计数器一旦到达
`BATCH_APPENDS`(64)，此后**每一次 append 都触发一次全文件 fsync**——
缓存退化成同步透传，彻底失去意义。

修复后（`sync_header` 末尾清零计数器，并加回归测试 `sync_resets_the_batch_counter`）：

| 指标 | 修复前 | 修复后 |
|---|---|---|
| IOPS | 113 | ~4100 |
| 写延迟 p50 | 500 ms | 5 ms |
| 写延迟 p99 | 1 s | 3.8 ms |

这解释了之前所有"同一命令手动跑和脚本跑差 10 倍"的怪现象：
不是测量问题，是**反复受环境状态（ublk 泄漏、磁盘满）干扰的测量**恰好落在
bug 表现的不同区间。

### 为什么在 NFS 上还是打不过直写

直写时，内核 NFS 客户端可以并行提交**几十个 RPC**；rcache 把所有队列线程
串行在**一把全局 `cache` 锁**上（写日志、回写、FLUSH 都要拿它）。
NFS 慢在单次 RPC 延迟而非并发能力，所以这把锁在这里是纯损耗。

其它已修复的、同样会拖慢写路径的问题：

- 回写步长默认 1024 块过大 → 改为 64 块，并加 20 ms 时间上限
- 写者遇到最高水位时无限等待 → 改为自己做有界排空
- 每步回写都对 origin `fsync`（NFS 上是同步 COMMIT）→ 改为最多 8192 块同步一次

### 对目标场景的推断

rcache 的设计目标是**机械盘**：随机写只有 0.5–2 MiB/s、且**不支持并发**。
那种场景下串行化不成为瓶颈，而把随机写变成顺序回写是真正的收益来源。
NFS 恰恰相反——它慢在单次 RPC 延迟，但能并发。

**要在这台机器上取得收益，必须先去掉全局锁**（分片锁或异步 IO）。
在此之前，上述实测不支持"NFS 上也有收益"这个说法。
