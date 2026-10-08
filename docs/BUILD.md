# 构建与开发

以下命令均在项目根目录执行。

固定依赖：Rust 1.97.1（`rust-toolchain.toml`）、cargo-ndk 4.1.2、Cargo.lock、npm package-lock、API 30，以及 `vendor/boringssl.lock.json` 中的 BoringSSL 修订和归档 SHA256。已验证 NDK 29.0.14206865，本地默认由 cargo-ndk 自动选择已安装的最新 NDK。原生密码库与 C++ runtime 静态链接，不调用系统私有 BoringSSL。

Windows、Linux（含 WSL）和 macOS 统一使用 `scripts/build.py`，目标始终是 **Android ARM64 ELF `altdb`**。安装 Rust / rustup、Python 3.12+、CMake 3.22+、Ninja，并将工具加入 PATH；用 Android Studio SDK Manager 安装 NDK（验证版本 29.0.14206865）。Rust 的宿主工具链需正常可用，以运行构建脚本和过程宏；原生 Windows 支持 Rust MSVC 或 GNU 宿主，Android C/C++ 编译由 NDK 完成。

```text
cargo install cargo-ndk --version 4.1.2 --locked
python scripts/build.py --binary-only
```

输出为 `target/aarch64-linux-android/release/altdb`，入口会下载并校验固定 BoringSSL、构建二进制并验证 ELF。仅构建 binary 不需要 Node.js、Go、Bash 或 WSL；Unix 上若 Python 命令名为 `python3`，替换命令或运行 `bash scripts/build.sh --binary-only`。Windows 可直接在 PowerShell / CMD 执行上述命令。

NDK 位于 Android Studio 默认 SDK 目录时无需设置路径；自定义 SDK 可设置 `ANDROID_HOME`，指定某个 NDK 可设置标准变量 `ANDROID_NDK_HOME`。无需指定 clang、linker 或 NDK 宿主子目录，原有 `ALTDB_NDK` 不再使用。[cargo-ndk 使用说明](https://github.com/bbqsrc/cargo-ndk/tree/v4.1.2)

需要完整模块时，再安装 Node.js 24 并运行 `python scripts/build.py`（Unix 也可用 `bash scripts/build.sh`）。它按锁文件构建 WebUI，生成 `dist/altdb-v0.1.3-arm64.zip` 及 `.zip.sha256`。包名从 Cargo 版本读取，并检查模块版本一致；支持通过 `CARGO_TARGET_DIR` 指定构建目录。ZIP 不包含 LICENSE、NOTICE 或依赖许可证目录，`scripts` 中没有 `.ps1`。

构建并通过 adb 安装到已连接的手机：

```text
python scripts/build.py flash
python scripts/build.py flash --reboot
python scripts/build.py flash -s 192.168.1.2:5555 --reboot
```

`flash` 始终先完整构建，上传本次生成的 ZIP，再调用 KernelSU 的 `ksud module install`；命令的 stdout / stderr 合并后实时转发到当前终端或 IDE 输出窗口。默认不重启，`--reboot` 仅在安装成功后执行普通重启。它会清理本次上传的临时 ZIP，安装失败不会重启。`--binary-only` 不能与 `flash` 同用。

也可直接运行等价的 binary 构建命令：

```text
python scripts/fetch-deps.py
cargo ndk -t arm64-v8a -P 30 build --locked --release --bin altdb
python scripts/package.py --check-binary
```

ELF 检查覆盖 AArch64、Android PIE / 动态链接器、16 KB PT_LOAD 对齐、动态 Bionic 和共享库白名单。16 KB 对齐同时兼容 4 KB 装载，但不能替代两种页大小真机运行验证。Linux / WSL 用于本机单元测试和原版 adb 回归；本机测试还需要 GCC/G++，WSL 内使用 Linux 工具链与 Linux NDK。macOS 交叉编译入口尚未实测。

RustRover / IDE 同步：项目的 `.cargo/config.toml` 默认选择 `aarch64-linux-android`，编辑器与普通 `cargo check --locked --all-targets` 使用真实 Android cfg 和标准库。同步时无需编译 BoringSSL 或设置 NDK 路径；构建脚本会提示仅检查 Rust 元数据，FFI 的编译与链接仍由实际 `cargo ndk` 构建验证。修改后在 RustRover 的 Cargo 窗口重新加载项目；若曾手动选择 Windows 分析目标，在右下角目标选择器切回 `aarch64-linux-android`。[RustRover 目标设置](https://www.jetbrains.com/help/rust/rust-cfg-support.html)

运行配置里的 `--target` 只作用于该命令，不能替代整个项目的分析目标。普通 `cargo build` 没有 cargo-ndk 的原生链接环境，会因缺少 `altdb_crypto` 原生库而失败；生成 Android binary 继续使用上面的构建入口。

开发验证（Linux；显式选择本机目标覆盖项目的 Android 默认值）：

```sh
python3 scripts/fetch-deps.py
export CARGO_BUILD_TARGET="$(rustc -vV | sed -n 's/^host: //p')"
cargo fmt --check
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked
python3 scripts/interop.py --adb /path/to/platform-tools/adb --binary "target/$CARGO_BUILD_TARGET/debug/altdb"
python3 scripts/test-service.py --adb /path/to/platform-tools/adb --binary "target/$CARGO_BUILD_TARGET/debug/altdb"
```

互操作测试使用独立 adb server、临时主机密钥和 Linux 回环测试入口，不操作实际手机。`test-daemon` / `test-ctl` 不出现在 Android CLI 中；Android 权限入口也拒绝测试模式。

`test-service.py` 在临时目录执行真实 service.sh 和 emulated-soft-reboot.sh，替换数据目录并模拟 getprop；覆盖开机等待期间的重复启动、未更新时软重启钩子 / boot_completed 重置 / HUP / 重复启动对同一 TLS shell 的影响、更新时旧 daemon 和看护脚本退出及模块目录替换后复用配对、开机等待和崩溃退避期间的更新停止，以及崩溃恢复和禁用/移除标记。它不模拟 Android 杀进程或网络重置，不能代替真机“模拟软重启”验证。

给互操作命令添加 `--latency-only` 可单独测量持续 shell 的单字节往返（40 次，报告 p50/p95）和新命令启动时间（8 次，报告 p50）。结果用于同一环境的版本比较，不代表手机 Wi-Fi 延迟。

模糊测试（Linux，沿用上面的本机目标变量）：先安装 cargo-fuzz 和 nightly，在仓库根运行 `python3 scripts/seed-fuzz.py`，然后 `cargo +nightly fuzz run --target "$CARGO_BUILD_TARGET" wire .cache/fuzz-corpus -- -max_total_time=60 -max_len=16384`。具体执行结果和局限见 [验收记录](VALIDATION.md)。

## 代码格式

`.editorconfig` 统一 UTF-8 / LF 与缩进。Rust 使用 `cargo fmt --all`；WebUI 与本地预览 JS 使用锁定的 Prettier 3.6.2，执行 `npm run format --prefix webui`，或以 `format:check` 只检查。Python 使用 Ruff 0.13.0 的 `ruff format scripts`，规则见 `ruff.toml`；C 使用 `clang-format -i native/crypto.c`，规则见 `.clang-format`。Shell 使用两空格缩进，分支与循环独立换行。格式化范围不含 vendor、生成的 WebUI 和构建产物。

## WebUI 语言

`webui/src/i18n.ts` 包含英文和简体中文词条、系统语言匹配与消息格式化。英文键集约束各语言的完整性。页面使用 `data-i18n` 标记静态文本，动态状态、通知和日期在渲染时根据当前语言生成；所有文本通过 `textContent` 写入。

语言偏好保存在 WebView 的 `localStorage`，默认跟随系统；存储不可用时，本次页面内仍可切换。CLI 和管理接口的内置消息始终为英文，WebUI 按英文消息标识翻译已知状态、日志和错误；未识别的诊断保留原文，主机名称等用户数据不翻译。

运行 `npm test --prefix webui` 验证语言回退、词条与占位符、动态消息及 CLI 文案；运行 `python scripts/preview.py` 可使用本地模拟 bridge 检查交互，`?scenario=paused` / `?scenario=offline` 分别模拟原生调试冲突和服务离线。预览不连接真实 daemon，不包含在模块中。
