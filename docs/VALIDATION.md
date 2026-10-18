# 交付验收记录

日期：2026-09-13。版本：0.1.3。**开发预览版，完整 Android 设备验收矩阵未完成。**

0.1.3 清理构建代码，移除 PowerShell 脚本和 Windows 运行目标；ZIP 不再包含 LICENSE、NOTICE 和依赖许可证目录。随后改用 cargo-ndk 4.1.2 和跨平台 Python 入口，支持 Windows 主机生成 Android ARM64 ELF；保留 Linux 本机测试目标。包名使用 Cargo 版本，并校验模块版本一致。

用户确认手机端问题已解决、已连接电脑。反馈未附完整 Android 版本、页大小、冷启动过程和延迟测量，以下专项验收不据此全部标记为通过。

### 2026-10-18：热安装启动 shell

在 Xiaomi 2512BPNDAC、KernelSU 32693 / UAPI 6 上确认：热安装后无 daemon 和 control.sock，service.stop 残留。用系统 sh 跟踪 service.sh 得到 `flock: Bad file descriptor`，随后脚本按锁冲突分支返回 0。`nohup sh` 会通过 PATH 执行系统 shell，不能保持 KernelSU standalone BusyBox 的文件描述符处理；原先使用普通 Linux shell 的回归未覆盖该差异。

修复为显式以 KernelSU BusyBox 执行停止钩子、nohup 和看护脚本。已开机时等待控制接口最多四秒，启动失败在安装日志中提示；daemon 最近一次启动的错误保存在私有 service.log。使用 KernelSU 自带 x86_64 BusyBox 的回环测试先复现旧版热启动失败，再验证修复后的启动、目录替换、单实例、配对保留和失败日志；普通 Linux shell 的兼容路径也通过。

手机上使用显式 BusyBox 启动现有看护脚本后，管理接口恢复，返回 0.3.0 / paused_native / Native wireless debugging is enabled；系统无线调试保留用于排查。本次未在手机安装新 ZIP，也未验证关闭原生调试后的连接恢复。Android ARM64 完整模块构建及 16 KB ELF 检查通过。

### 2026-10-08：shell 启动环境与 scrcpy

在 Xiaomi 2512BPNDAC / Android 17、KernelSU 32667 / UAPI 5 上复现：旧版 shell 缺少 `ANDROID_DATA` 等变量，`app_process` 报 `ANDROID_DATA environment variable unset`，scrcpy 5.0.1 报 `Server connection failed`。修复 daemon → service → shell 的运行时环境白名单传递，并补齐系统 PATH、TMPDIR、LOGNAME；通过保留原看护进程的启动环境重启 daemon，设备运行修复后的二进制。

- 真机 scrcpy 默认 reverse 路径、强制 `--force-adb-forward` 路径均完成限时录制。ffprobe 确认前者有 1200×2608 H.264 视频 29 帧，后者有同分辨率视频 24 帧及 Opus 音频 404 帧；录像为 `.cache/scrcpy-after.mkv`、`.cache/scrcpy-after-forward.mkv`。音频初始 timestamp 警告未阻止录制；未验证键鼠注入或音频听感。
- 真机 shell/root 模式均保留 Android 目录、BOOTCLASSPATH、TMPDIR 和对应 USER/LOGNAME；测试后恢复 UID 2000 / shell。`ASH_STANDALONE`、`KSU` 未进入普通 shell。
- 35 项 Rust 测试、严格 Clippy、Rustfmt、Ruff、Android ARM64 release 构建及 16 KB ELF 校验通过。
- 新增原版 adb 回归覆盖普通 shell、PTY 和 exec-out：设备 runtime/classpath 值跨两次 exec 保持，KernelSU / loader / 任意测试变量不泄漏。完整回环回归通过，包括并发 shell、SYNC、转发、root/unroot、撤销和重启。
- WSL 直接执行 `/mnt/d` 上的 debug 二进制时，并发 shell 回归出现退出码 255；将同一个二进制复制到 Linux `/tmp` 后，新增环境回归及完整原版 adb 回归全部通过。对照提交 `7927f51` 在 Linux `/tmp` 下也通过；此处不把 Windows 挂载目录上的失败归因于环境变量修复。

本次真机测试替换模块中的二进制并重启 daemon，未执行整机冷启动或模拟软重启，也未据此补齐下方完整设备验收矩阵。

### IPv4、排版与模拟软重启更新

连接、配对、接口发现和 mDNS 已统一为 IPv4；移除 IPv6 地址与 WebUI 的 scope 提示。Rust、C、TypeScript、HTML、CSS、JSON、Python 和 Shell 已整理缩进与换行，预览 JS 从 Python 字符串中移为独立文件，并加入格式配置。

软重启处理已简化为设置读取失败的有限宽限，不跟踪框架进程。已有运行实例可使用最近一次成功检查的结果，最长 120 秒，失败不刷新时间；原生调试启用、/proc 读取失败和检查过期仍暂停。service.sh 在开机等待前取得独立文件锁，重复执行直接退出，daemon 不继承脚本锁。

本轮本地验证：

- Linux / WSL 的 34 项 Rust 测试与严格 Clippy 通过，覆盖 IPv4 地址拒绝、mDNS A 记录、设置读取宽限与超时/冲突优先级、设置命令及继承 stdout 的超时回收检查。框架进程跟踪的专项测试已随实现移除，简化后的验证日志见 `.cache/verify-monitor-simple.log`。
- 原版 adb 完整回环回归通过：配对、TLS、并发 shell、二进制 IO、SYNC、转发、权限切换、撤销与重启。新增真实 service.sh 回归通过：开机前重复启动、运行中 boot_completed 重置 / HUP / 重复启动时同一 TLS shell 和端口保持、崩溃恢复及禁用/移除。日志：`.cache/verify-ipv4-linux.log`。
- Rustfmt、Prettier、Ruff format、clang-format、Python / Shell / JS 语法检查通过；4 项 WebUI 测试与 TypeScript / Vite 构建通过。
- Windows cargo-ndk 构建 Android ARM64 ELF 与模块 ZIP；产物使用 16 KB 加载段对齐，校验和见随包 `.zip.sha256`。

尚未在手机执行本轮“模拟软重启”、确认 KernelSU BusyBox 的脚本锁行为及网络保留情况。Linux 测试不替代真实 Android 进程生命周期验证；完整 userspace reboot 或系统终止 altdb、重置网络时无法保留原 TCP 连接。历史记录中提到的 CI 配置与部分缓存日志已由工作区清理，当前以本轮记录和现存源码为准。

0.1.2 针对设备反馈修复首次启动时空 `init.svc.adbd` 属性的误判，结合设置与实际进程检查；系统设置检查移到独立线程，检测未完成、失败、过期时仍暂停。针对 shell 输入/响应延迟，启用 TCP_NODELAY、合并 ADB 包写入，并使用 IO 就绪通知。

0.1.1 修复用户设备报告 UAPI 4 时被 `uapi_version == 2` 误拒绝的问题，改为 UAPI >= 2，不设上限；保留最低内核版本和独立进程禁止提权探测，报错包含实际 UAPI 和最低要求。新增 UAPI 2/3/4/5/更高版本与元数据标志的接受检查，以及旧内核、UAPI 0/1 的拒绝检查。su 隔离仍需专项设备测试记录。

## 已执行

| 检查 | 环境与结果 |
| --- | --- |
| 配对与 TLS 基础能力 | Windows 原版 adb 37.0.1：`adb pair` 成功，`adb connect` 成功，`get-state` 返回 device |
| 协议与平台无关单元测试 | Linux / WSL：0.1.3 共 29 项通过，覆盖首次空 adbd 属性、未设置/不可读取的调试开关、后台检测不阻塞及过期暂停、部分写入后追加 WRTE 的顺序与队列上限，以及 UAPI、包解析、密码学、SYNC、DNS、FD、接口和持久化回归 |
| 静态检查 | Linux Rustfmt、`cargo clippy --locked --all-targets -- -D warnings`、Shell / Python 脚本语法检查通过 |
| 模糊测试 | cargo-fuzz 0.13.2，Rust 1.100.0-nightly (809936eac)，libFuzzer + AddressSanitizer；有种子的 ADB / shell / JSON 管理请求 / 主机公钥 / DNS 解析，61 秒、241,158 次输入，未崩溃。固定协议常量附近的有限测试，不等同于完整密码学或 C FFI 审计 |
| 原版 adb 端到端测试 | Linux adb 37.0.1，私有 adb server、临时主机密钥、仅回环测试 daemon；0.1.3 标准完整回归通过，见 `.cache/verify-0.1.3-linux.log`。0.1.2 另使用 `scripts/interop.py --exec-in-repeats 500` 完整通过，500 次 1 MiB+7 随机二进制输入逐字节比较一致 |
| shell / exec | stdout / stderr 分离、退出码 19、PTY、NUL/0xff 二进制 exec-out、1 MiB exec-in 完整性、忽略 HUP 的会话断线后回收 |
| 流复用 | 单连接 16 路并行，32 个 shell 任务分别正确返回 |
| 文件传输 | SYNC v2、2 MiB+ 随机二进制 SHA256 一致、目录、空文件、含空格/中文路径 |
| 转发 | TCP / localabstract forward；TCP / localfilesystem reverse；删除监听和文件路径清理 |
| 管理与认证 | 未配对主机拒绝、全局 root 策略/稳定端口/unroot、撤销后断线并拒绝重连、十次错码关闭、身份跨进程重启、固定端口占用、禁用/启用 |
| WebUI | TypeScript / Vite 构建；375 px 本地浏览器模拟检查配对开关、倒计时、固定端口、日志、纯文本主机名称，无浏览器错误。使用模拟 KernelSU bridge，不代表真实 Manager 验证 |
| Windows → Android ARM64 | Windows 原生 Rust 1.97.1 GNU 宿主、cargo-ndk 4.1.2、NDK 29.0.14206865 自动发现；`python scripts/build.py --binary-only` 与完整模块构建均通过，无需 WSL / Bash / Go。输出 Android API 30 PIE，静态 BoringSSL/C++ runtime，动态依赖仅 libc.so / libdl.so，四个 PT_LOAD 均为 16384 字节对齐；ZIP 和 SHA256 已生成 |
| Linux → Android ARM64 | Linux / WSL 使用 cargo-ndk 4.1.2、NDK 29.0.14206865 与 `bash scripts/build.sh --binary-only` 通过；设置 `ANDROID_NDK_HOME` 和 `CARGO_TARGET_DIR`，无需自设 linker。Android PIE、动态库依赖和四个 16 KB 加载段检查通过 |

Linux 测试入口有意不执行 Android UID / SELinux / KernelSU 切换。上表中的 root 测试证明管理策略和重连行为，**不能作为手机 root 隔离通过的证据**。浏览器预览服务不打入模块。

### cargo-ndk 构建记录（语言更新前）

此前调整构建工具链、入口和交付检查时，服务与密码协议源码保持 0.1.3。Windows 日志：`.cache/build-windows-cargo-ndk-final.log`（binary）与 `.cache/build-windows-cargo-ndk-module.log`（完整模块）。Linux 日志：`.cache/verify-cargo-ndk-linux.log`，当轮 Rustfmt、29 项单元测试、Clippy 严格检查、原版 adb 完整回归和 Android 交叉编译全部通过。当时的 Windows 二进制 SHA256 为 `fe1e34f24c2f6388a3f2285994ba76d6fd0f3ce00f648d73e9df8871f3aa7d51`；当前产物的校验和以随包 `.zip.sha256` 为准。

CI 已添加 Windows MSVC 宿主的 Android binary 构建任务，保留 Linux 完整构建与回归；CI 配置尚未在远程 Actions 执行。Windows MSVC 和 macOS 主机尚未本地实测。本轮新构建的 Android ELF 未部署到手机，既有设备反馈不能替代该产物的设备验收。

### RustRover 同步修复

项目默认分析目标设为 `aarch64-linux-android`；没有 cargo-ndk 原生构建环境时，build.rs 仅提供检查元数据所需的链接声明，不执行 BoringSSL 构建。Windows 原生 `cargo check --locked --all-targets` 通过，覆盖 Android 库、主程序、测试和示例；日志为 `.cache/check-rustrover-android.log`。普通 `cargo build --locked --lib` 在同一环境按预期因缺少原生 `altdb_crypto` 而失败，证明检查路径未引入可运行的占位实现。

随后 `python scripts/build.py --binary-only` 真实构建与 ELF 检查通过，Android 二进制 SHA256 与前述产物一致，见 `.cache/build-rustrover-fix-android.log`。CI 增加 Windows 默认目标的无原生依赖检查，Linux CI 和文档改为显式选择宿主目标。RustRover GUI 中重新加载后的状态尚待用户确认；已验证其底层 Cargo 检查命令。

显式 Linux 宿主目标的 29 项单元测试、Clippy 严格检查、开发二进制构建及 `--version` 执行通过，日志为 `.cache/verify-ide-linux.log`；没有增加 Windows 服务实现或协议占位代码。

### WebUI 多语言与英文 CLI

README 已改为简洁英文模块说明，不包含构建内容。WebUI 支持英文和简体中文，默认匹配系统语言，可手动选择并保存；CLI 和管理接口的内置状态、日志与错误均使用英文。WebUI 翻译已知服务消息，未知诊断和用户提供的主机名称保持原文。

本轮验证：

- 4 项 WebUI 测试通过：语言回退与覆盖、词条和占位符完整性、动态消息安全插值、CLI 内置消息语言检查；TypeScript 和 Vite 构建通过。
- 本地浏览器模拟 bridge 验证中英文即时切换、刷新后保留选择、未保存端口不丢失、配对倒计时、日志和错误提示翻译、原生调试暂停与服务离线。375 px 视口无横向溢出，未发现浏览器脚本错误；主机名称和未知日志中的 HTML 保持纯文本。
- Linux 的 29 项单元测试、Clippy 严格检查、开发二进制构建和英文帮助输出通过，见 `.cache/verify-i18n-linux.log`；Windows 默认 Android 目标的 `cargo check --locked --all-targets` 通过。
- Windows 生成 Android ARM64 ELF，四个加载段均为 16 KB 对齐，动态依赖仅 libc.so / libdl.so；见 `.cache/build-i18n-android-final.log`。二进制 SHA256：`054858174e41d7c589ef2bbd81b8adb77dabdfe3eff02ef16d5fbbb6db608ad5`。

真实 KernelSU WebView 的系统语言与存储行为，以及这次产物的手机运行仍待设备验证。

### shell 延迟对比

同一 WSL / Linux 回环环境、原版 adb 37.0.1、debug 二进制。修复前使用保留的 0.1.0 二进制（其网络路径与 0.1.1 相同）；修复后为 0.1.2。持续 `shell -T cat` 先预热一次，再测 40 次单字节往返；另测 8 次独立 `shell printf ok`。这是本地版本比较，不能当作手机 Wi-Fi 实测或对原生无线调试的比较。

| 指标 | 修复前 | 0.1.2 |
| --- | ---: | ---: |
| 持续 shell RTT p50 | 47.99 ms | 0.48 ms |
| 持续 shell RTT p95 | 52.06 ms | 0.64 ms |
| 独立命令完成 p50 | 117.94 ms | 21.60 ms |

加速后压力测试发现原版 adb 会在前包部分写入时交付后续 WRTE，原实现因此断开连接。现用有界包队列保留偏移与顺序，并以确定性单元测试和 500 次大文件输入验证。测试日志保存在工作区 `.cache/latency-before.log`、`.cache/interop-0.1.2-final.log`，不打包测试服务。

## 待完成的设备验证

已收到手机连接正常的用户反馈；以下项目尚无完整的设备测试记录，不能标记为通过：

- Android 11 与较新 Android 版本，分别覆盖 4 KB 和 16 KB 页大小。
- 官方 KernelSU 3.2.5+ 的版本/UAPI/禁止提权安装拒绝与启动复查，包括内核降级。
- shell 的 `su` 提权遵循 KernelSU 授权；分别在授权和未授权时覆盖子 shell / exec / PTY。网络进程仍设置可继承的禁止提权标志。
- enforcing SELinux 下网络进程 `u:r:altdb_net:s0`、UID 2000、capabilities 清零；普通服务 shell 域与附加组；root 服务 `u:r:ksu:s0`。检查继承的 ksu 标签 TCP/UDP/Unix socket 与 shell 标签 reverse socket 的实际 AVC。
- adb root 开关的启用和禁用、多电脑同时连接、关闭 adb root 立即停止会话和 root 模式；确认它不改变 KernelSU 的 shell 授权。
- 真实 `install` / `install-multiple` / `uninstall`、logcat、reboot、shell v2 终端 resize、SYNC v1 客户端。
- 原生 USB / 无线调试开启、关闭、adbd 残留/重启和关键状态不可读；暂停时监听、会话、转发及 mDNS 全部关闭。
- 从未开启原生调试的冷启动，无需先开关调试即可确认状态；同一手机/网络下复测交互式 shell 输入延迟和新命令响应。
- Wi-Fi / 热点 / 以太网切换，IPv4 多地址、mDNS 自动重连，确认 IPv6 / 蜂窝 / VPN 无监听。
- 真实 KernelSU WebUI exec、模块安装/升级/禁用/卸载、磁盘满/只读失败和异常断电后的持久化。
- 长时间吞吐、持续恶意握手与并发流资源占用、系统压力下的退避和清理。

## 两项优先验收

### 1. 原版 adb 配对和 TLS 重连

在手机关闭原生调试，WebUI 开配对窗口。电脑执行页面命令后分别测试 shell、断开再连接、模块重启后不重新配对连接。未知主机密钥必须被拒绝，通信端口上的明文 ADB AUTH 不得成功。

回环版自动测试已验证该协议路径；真机仍需要验证 SELinux 与实际网络。

### 2. KernelSU 禁止提权标志与 shell 授权

先在 KernelSU Manager **手动授权 shell**，然后从手机本地 root 终端执行：

```sh
/data/adb/modules/altdb/bin/altdb doctor --verify-su-isolation
```

命令在两个独立进程中使用与普通服务相同的 UID/GID、capabilities 和 shell SELinux 域：基线允许 su 必须得到 UID 0；诊断进程额外设置 KSU_NO_NEW_PRIVS 后的 su 必须失败或不能得到 UID 0。只有两项均满足才返回 `ksu_no_new_privs_blocks_su: true`。普通 shell 服务不设置此标志，网络进程仍设置；该诊断验证内核标志，不替代网络进程实际身份和 SELinux 检查。命令不更改 KernelSU 的 shell 授权或模块配置。基线未获 root / 超时表示“前置条件未满足”，不等价于隔离成功。

再经真实配对连接测试：关闭 adb root 时 `adb shell id` 应为 UID 2000，`adb shell su -c id` 的结果取决于 KernelSU 的 shell 授权；允许 adb root 后再执行 root，TLS 连接保持，新服务应为 UID 0。执行 unroot 后新服务应为 UID 2000，已有服务保持原权限；冲突恢复或服务重启后也应回到 shell 模式，shell 使用 su 仍遵循 KernelSU 授权。

## 复现实验与记录

使用方法见 [README](../README.md)，开发验证命令见[构建与开发](BUILD.md)。每次设备验收记录 Android 版本、内核、KernelSU Manager/内核版本、页大小、设备网络接口、模块 SHA256、测试命令和结果；不要记录配对码、私钥、实际命令内容或文件内容到模块诊断日志。

已修复并纳入端到端回归的缺陷：raw exec 的 socket→pipe splice 导致 shell 退出卡住；PTY parent 持有 slave；exec-in 最后 WRTE 与 CLSE 同批到达丢尾部；主机撤销与认证回执竞争；配对先确认后落盘的顺序；持续背压时 TLS 已消费缓冲前缀未释放。

模块 ZIP 是用于继续真机验收的产物。通过 ELF 对齐检查不能证明已经在 16 KB 设备运行，静态检查和有限模糊测试不能替代安全审计。
