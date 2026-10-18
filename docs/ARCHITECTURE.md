# 架构和边界

## 进程

```text
KernelSU WebUI ── exec 固定 CLI ── root-only control.sock
                                      │
                         UID 0 / ksu daemon / 管理与监听
                            │ 私有 SOCK_SEQPACKET + SCM_RIGHTS
                            ├── UID 2000 / altdb_net 网络进程
                            │     TLS / SPAKE2 / ADB 流 / mDNS
                            └── 每服务独立子进程
                                  UID 2000 / shell，或显式 UID 0 / ksu
```

root 管理进程创建监听套接字，检查接口、原生调试、配置、主机授权，保存状态并启动服务；不解析网络上的 ADB、TLS 或 SYNC 数据。私有 IPC 对报文、FD 数量、操作种类作限制。服务子进程通过一次性的 fd 3 bootstrap 接收数据 socket，关闭 bootstrap 后，在处理服务数据和启动命令前切换身份。

网络进程先设置 KernelSU 禁止提权，清空 capabilities / bounding set、降到 UID/GID 2000，再进入 `altdb_net` 域并设置 Linux no_new_privs。网络进程只有必要的已打开 socket 使用权限，不进入 shell / ksu，不读取持久化目录。TLS 私钥通过 root 私有 IPC 进入网络进程，磁盘副本仅 root 可读。

普通服务采用 AOSP shell 基础附加组；在离开 ksu 域前完成 UID/GID 和 capabilities 降权，再进入 shell 域。shell 能否通过 `su` 提权由 KernelSU 的 shell 授权决定。root 模式只改变服务身份，网络进程始终保持禁止提权。

启动服务和 shell 时先清空环境，再从父进程按白名单传递 Android 运行时目录、存储目录和 classpath；`BOOTCLASSPATH` 等沿用设备 init/APEX 提供的值，不硬编码框架 jar 列表。这样保留 `app_process` / scrcpy 所需环境，同时不继承 `LD_*`、`CLASSPATH`、shell 启动钩子或 KernelSU 的 `ASH_STANDALONE`；客户端可在自己的 shell 命令中显式设置 `CLASSPATH`。网络进程不接收这组运行时变量。shell 的基础 PATH 包含系统及 ART/runtime APEX 等目录，并设置 HOME、TMPDIR、SHELL、TERM、USER 和 LOGNAME。root shell 在 PATH 末尾追加 `/data/adb/ksu/bin`，且在 `/data/adb/ksu/.ksurc` 为文件时显式设置 `ENV` 指向它，由交互式 shell 加载；非交互式 `shell -c` / exec 命令不执行 rc 文件。`adb unroot` 后新建 shell 恢复基础环境。daemon 应由正常 KernelSU 启动环境拉起。平台依据：[init 环境](https://android.googlesource.com/platform/system/core/+/refs/heads/main/rootdir/init.environ.rc.in)、[adbd shell 环境](https://android.googlesource.com/platform/packages/modules/adb/+/refs/heads/main/daemon/shell_service.cpp)、[KernelSU root shell](https://github.com/tiann/KernelSU/blob/main/userspace/ksud/src/su.rs)。

官方 KernelSU 的 root context 为 `u:r:ksu:s0`。KernelSU 自带策略已将 ksu 设为 permissive，并允许其他 domain 使用其 FD、发送 SIGCHLD，以及进行 Unix stream socket 的 read/write/connectto/getopt/getattr；模块不重复添加这些规则。permissive 作用于执行操作的源域，不会自动免除 enforcing 网络进程对 ksu 标签 socket 的检查。管理进程创建并传递的 TCP/UDP socket，以及 Unix socket 的 setopt/ioctl/shutdown，仍由模块按具体操作授权；reverse 监听由 shell 创建，单独保留对 shell 标签 socket 的使用权限。依据：[KernelSU v3.2.5 域定义](https://github.com/tiann/KernelSU/blob/v3.2.5/kernel/selinux/selinux.h)、[内置策略](https://github.com/tiann/KernelSU/blob/v3.2.5/kernel/selinux/rules.c)。

root 服务本身具有设备 root 权限，能修改系统与模块；不把允许 adb root 描述成沙箱。独立非 KernelSU root 通道不属于 KSU_NO_NEW_PRIVS 的保护范围。

安装和启动要求 KernelSU 内核版本 32525+、UAPI >= 2，不设 UAPI 上限；同时在独立进程实际调用 `KSU_IOCTL_DISABLE_ESCAPE_TO_ROOT`，探测失败仍拒绝运行。UAPI 3 引入 scoped su-session FD，UAPI 4 新增 BUNDLED 信息标志；altdb 使用的 16 字节 GET_INFO 结构、查询 ioctl 和禁止提权 ioctl 保持兼容。flags/features 是元数据，不作为禁止提权能力证明。低于最低要求时，报错包含实际内核/UAPI 版本和最低要求。接口依据：[v3.2.5](https://github.com/tiann/KernelSU/blob/v3.2.5/uapi/supercall.h)、[UAPI 4](https://github.com/tiann/KernelSU/blob/main/uapi/supercall.h)；源码兼容性核对不替代真机的 su 隔离验收。

网络进程在技术上也可以从 ksu 切换为 `u:r:adbd:s0`，同时保持 UID 2000 和 capabilities 清零。复用意味着采用 ROM 给原生 adbd 的整套 SELinux 授权，其中包括 USB、调试文件和系统属性等与纯网络解析无关的访问；本实现按独立网络域的边界使用 `altdb_net`。仅替换进程 context 不会重标记管理进程已创建的 socket，复用 adbd 时也需要核对继承 socket 的标签和权限。参见 [AOSP adbd 策略](https://android.googlesource.com/platform/system/sepolicy/+/refs/heads/main/private/adbd.te)。

## 配对和认证

- 配对 TLS 1.3；TLS exporter 为 `adb-label\0`，导出 64 字节。
- 六位码与 exporter 一起作为 SPAKE2 密码；AOSP Alice/Bob 名称包含结尾 NUL。
- 使用 BoringSSL SPAKE2，HKDF-SHA256、AES-128-GCM；nonce 为 12 字节，其中前 8 字节是独立的 LE 收发计数。
- PeerInfo 为 8192 字节：电脑发送 Android RSA 公钥；设备返回持久 GUID。
- 验证主机 PeerInfo 并成功保存授权后，才发送设备 PeerInfo 作为成功响应；保存失败不会向电脑确认配对。
- 连接从 CNXN / STLS 升级至 TLS 1.3，客户端证书公钥的 SHA256(SPKI) 必须存在于授权记录。没有旧式 AUTH/明文回退；关闭 session cache / tickets。
- TLS 握手未完成的连接十秒后被网络主循环关闭；单次配对尝试二十秒，整体窗口五分钟。

授权撤销同时删除磁盘记录、结束 root 管理的该主机会话，并通知网络进程关闭连接。网络侧在认证请求发出前登记主机身份，避免授权更新与认证回执之间的竞态；root 侧每次启动服务再次核对连接授权。

## 限额与背压

ADB 包最长 1 MiB，协商不超过实现上限；输出块 64 KiB，每流等待 OKAY 后继续。每连接最多 64 个流、最多 8 MiB 待写输入；TLS 排队输出上限 8 MiB，达到 4 MiB 暂停读取服务输出。最多 8 个 TCP transport、32 个已配对公钥、每连接 16 个 reverse 监听。

每流持有服务 socket；已接收的 WRTE 队列排空到服务 socket 后回复 OKAY。原版 adb 偶尔会在前包尚未排空时发送后包，因此保留有界的包队列和前包的部分写入位置；每流最多 64 个待写包，每连接保留的输入数据（包括部分写入包的已消费前缀）不超过 8 MiB，已排空包立即释放。CLSE 会先排空已经接收的数据，再向服务传递 EOF，解决 exec-in 最后数据丢失问题。普通流关闭给服务最多一秒结束宽限，随后回收会话；撤销、冲突、收紧权限会立即停止受管进程组。服务启动独立子进程组，在退出和断线时清理同组后代。

ADB TCP 和配对连接启用 TCP_NODELAY；ADB 包头和负载合并后一次交给 TLS，避免原来七次小写入产生多个微小 TLS record。连接等待 TCP、服务 socket 和 reverse listener 的 IO 就绪通知，管理进程等待私有 IPC 与 CLI 的就绪通知；空闲超时只用于定期检查和清理，不给活跃请求固定增加 sleep 延迟。等待 ADB OKAY 的流不监听服务 EOF，保持输出确认与关闭的顺序。

PTY 的 slave FD 在 spawn 后全部关闭；shell v2 支持 stdin / close-stdin / resize 和 stdout / stderr / exit。原始 socket 到 stdin pipe 使用有界 read/write；不使用会在等网络输入时持有 pipe 锁的 splice 路径。

SYNC 子进程按 shell/root 权限访问文件。路径最长 1024 字节，DATA 最长 64 KiB；普通 push 写同目录随机临时文件，完整接收后 rename；断线删除临时文件。不支持压缩或设备节点写入。

## 网络与生命周期

ADB 连接启用 TCP keepalive（空闲 30 秒后探测、间隔 10 秒、三次未响应）及 60 秒 TCP_USER_TIMEOUT，清理网络突然中断或电脑离线后遗留的连接；正常空闲连接由 TCP 自动应答，不因没有 ADB 命令而断开。网络主循环统一回收退出的连接线程并通知管理进程清理计数和会话，覆盖认证回执失败等提前返回路径。

接口由 getifaddrs 和 sysfs 类型、无线/物理设备信息筛选；只接受 IPv4，TCP 绑定具体地址和 SO_BINDTODEVICE。所有接口在同一启动周期使用相同通信端口，mDNS 仅发布 A 地址记录。新建监听失败时全部回滚。

mDNS 监听按获准接口绑定 IPv4 UDP 5353，发布 `_adb-tls-connect._tcp` / `_adb-tls-pairing._tcp`、SRV、TXT、A；名称为持久 GUID。支持有界 DNS 压缩指针解析、周期公告、查询回应和 TTL 0 撤销。跨厂商 mDNS / 多网卡和电脑自动连接仍需实机互操作验收。

属性变化通过 Android property wait 通知主循环，并有 250 ms 兜底检查。独立检查线程每次检查完成后三秒重新读取全局调试设置和 /proc 中实际 adbd 进程；系统命令限时两秒，避免阻塞管理进程的认证、启动服务和 CLI。首次结果尚未就绪、读取失败或最近结果超过六秒时暂停。未启动过的 adbd 可能没有 `init.svc.adbd` 属性，此时只有设置读取成功、调试开关未开启且进程检查无 adbd 才能运行；明确的 running/restarting/stopping 或 USB/无线开启仍立即触发暂停。残留 TCP 端口属性不单独构成冲突。

冲突关闭配对、连接、转发、mDNS 和监听，保留管理 socket；恢复回到 shell 并重建监听。全局 root / unroot 更新新服务使用的权限，保留网络进程、TLS 连接、监听和已有服务；新服务在启动时读取当前模式，已有交互式 shell、SYNC 和转发服务保持原权限。

设置读取失败时，对已有服务使用最近一次成功确认调试关闭的结果，最长 120 秒，失败不刷新该时间。宽限期间保留工作进程、会话、权限模式和端口，暂缓网络快照重建；WebUI 显示设置暂时不可读。原生调试开关与实际 adbd 进程仍持续检查，确认冲突、进程读取错误、后台检查过期或超过宽限期时暂停。首次启动不能使用宽限，设置恢复后重新验证并恢复网络检查。这样允许框架软重启期间设置服务暂时不可用，无需跟踪框架进程。

设置读取的两秒限时覆盖命令退出和 stdout 排空，失败时终止独立进程组，防止框架重启期间遗留的 cmd 子进程或管道阻塞检查线程。完整 userspace reboot 会终止用户空间进程；系统杀掉 altdb、重置网络或内核重启时无法保留原 TCP 连接。[AOSP 软重启执行过程](https://source.android.com/docs/core/runtime/soft-restart)

配置和主机列表修改采用私有同目录临时文件、fsync 和 rename；内存配置在保存成功后更新。daemon 与 service.sh 分别持有 daemon.lock / service.lock 的 flock。脚本在等待 boot_completed 前取得锁，重复调用直接退出；锁放在持久数据目录，避免模块更新替换目录后产生第二个看护进程。daemon 不继承脚本锁，崩溃后由原看护进程退避重启；HUP 不停止看护进程。禁用/卸载标记及 TERM/INT 会触发停止，进程退出自动释放锁，无需删除锁文件；模块不覆盖系统文件。

安装器提供 `KSU_HAS_HOT_INSTALL=true` 时，customize.sh 请求热安装并指定 hotinstall.sh。KernelSU 换入新模块目录、加载 sepolicy.rule 后执行钩子；钩子强制停止旧 daemon 和看护脚本，等待两个持久锁释放，再以独立标准输入输出后台启动新版 service.sh。停止与启动均显式使用 `/data/adb/ksu/bin/busybox sh` 并保留 standalone 模式，避免 nohup 经 PATH 启动系统 sh 后丢失 flock 等 applet。停止阶段的 CLI 限时五秒，两个锁共用二十秒等待预算；已开机时再给控制接口四秒就绪时间，失败则在安装日志提示。daemon 最近一次启动的输出保存在私有目录的 service.log。热安装会断开现有会话，配置、身份和配对记录保留；不支持热安装的安装器继续走重启流程。接口依据：[KernelSU 热安装提交](https://github.com/tiann/KernelSU/commit/200818cabf8ff8ba857dee068c5fae5ae63ae6aa)。

[KernelSU 的软重启流程](https://github.com/tiann/KernelSU/blob/main/userspace/ksud/src/soft_reboot.rs)在替换待更新模块前执行 `emulated-soft-reboot.sh`。没有 `update` 标记时直接返回，保留 daemon 和现有会话；有标记时写入持久目录中的 `service.stop`，通过 `ctl stop` 请求 daemon 清理退出，并等待看护脚本和 daemon 的锁释放。看护脚本在等待开机、运行和崩溃退避期间均响应停止请求，新看护脚本取得锁后才清除该请求。走重启更新路径时，仅安装更新不会立即停止服务。不支持热安装时，首次从不含此钩子的旧版本升级应完整重启一次，使后续软重启能调用已安装的钩子。

## 代码导航

- `native/crypto.c`、`src/crypto.rs`：固定 BoringSSL FFI 与安全所有权。
- `src/pairing.rs`、`src/protocol.rs`：配对线协议、ADB 包和 shell 帧。
- `src/unix/daemon.rs`、`worker.rs`、`ipc.rs`：权限边界、管理和 IPC。
- `src/unix/transport.rs`：流复用、ACK、reverse。
- `src/unix/services.rs`、`services/sync.rs`：独立服务、PTY 和 SYNC。
- `src/unix/ksu.rs`、`network.rs`、`mdns.rs`、`store.rs`：平台能力与持久化。
- `webui/`、`module/`、`scripts/`：本地管理页面、模块脚本、构建和互操作测试。
