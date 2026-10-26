# 为 CentOS 7.6 构建原生服务端

[English（规范版本）](<centos7-build.md>) · [服务端配置](<README.md>)

本方案在 CentOS 7.6 用户态环境中，将当前源码构建为**内嵌 WebUI** 的 Linux x86_64 原生服务端。Docker 只是构建依赖，目标 VPS 运行二进制不需要 Docker。构建镜像**不是生产服务镜像**：以下命令不会安装系统服务，也不会自动在 VPS 上启动 Nyro。

> CentOS 7 已停止维护。ABI 兼容不等于恢复安全支持；优先迁移到仍受支持的操作系统，将本方案视为兼容过渡措施。提供构建方案和检查步骤，不代表某次构建已经在你的 VPS 上验证通过。

本方案已完成一次端到端验证：产出的 `nyro-server` 在 CentOS 7.6.1810 主机（内核 `3.10.0-957`、glibc `2.17-260`）上成功启动，回环地址的健康/就绪检查返回正常，并正确提供内嵌 WebUI。该结果只覆盖当前源码版本与工具链组合，不代表后续改动或其他主机；你自己的构建仍需重新执行下文检查。

## 前提与构建

构建主机需要 Docker、Buildx 及 Docker 守护进程访问权限。**推荐本地 Linux AMD64 构建器**。脚本固定请求 `linux/amd64`，ARM 主机需要可用的 AMD64 模拟环境，构建速度可能明显下降。即使不使用 `sudo`，Docker 访问权限也具有较高权限。

构建主机和 VPS 均不需要安装 Rust 或 Node。首次构建需要访问容器镜像仓库、CentOS Vault、Rust 分发站、crates.io 和 npm registry，并预留足够磁盘与内存。编译器因内存不足被终止时，降低并行任务数。

在仓库根目录执行：

```bash
./scripts/build-centos7.sh
# 等价命令：make server-centos7

# 限制 Rust 编译并行度，并指定导出目录。
./scripts/build-centos7.sh --jobs 2 --output "$PWD/target/centos7"

# 只准备可复用工具链镜像，不编译 Nyro 或 WebUI。
./scripts/build-centos7.sh --image-only

# 复用已有 Cargo registry，并禁止此次 Cargo 构建访问网络。
./scripts/build-centos7.sh --cargo-registry "$HOME/.cargo/registry" --offline
```

[构建脚本](<../../scripts/build-centos7.sh>) 支持：

| 参数 | 默认值 | 含义 |
| --- | --- | --- |
| `--jobs N` | 环境变量 `BUILD_JOBS`，否则 `4` | Rust 编译并行任务数，必须为正整数；不限制前端构建并行度。 |
| `--output DIR` | 仓库内的 `target/centos7` | 导出目录（因 Buildx 使用 CSV 语法，不接受含逗号的路径）。自定义相对路径相对于调用脚本时的工作目录。 |
| `--image-only` | 关闭 | 只构建并加载可复用工具链镜像，不导出服务端二进制。 |
| `--image NAME` | `nyro-builder:centos7.6-rust1.96.0` | 已加载构建镜像的标签；不用于切换基础镜像或工具链。 |
| `--cargo-registry DIR` | 无 | 用可信的本地 registry 目录填充容器 Cargo registry，必须包含 `index/` 和 `cache/`。传入 registry 目录，而不是整个 `CARGO_HOME`。相对路径基于调用时的工作目录，支持空格。 |
| `--offline` | 关闭 | 仅为 Cargo 编译设置 `CARGO_NET_OFFLINE=true`；不会使 Docker、工具链或前端步骤离线。 |
| `--help` | — | 显示帮助。 |

默认流程先加载工具链镜像，再构建产物阶段。后续调用可复用同一构建器的 BuildKit 层和 Cargo 缓存。仅构建镜像并不保证已缓存离线源码编译所需的全部 Cargo 依赖。

### 利用缓存重新构建 Cargo

`--cargo-registry` 为可选参数，默认构建允许 Cargo 下载依赖。指定后，该目录作为 `cargo-seed` Buildx 上下文传入，复制到容器 registry 缓存，且不覆盖已有条目。应使用包含所需索引条目和 crate 归档的可信 Cargo registry，也可以包含已解压源码。不要在该目录放入凭据或无关私有文件：所选 Docker 构建器能够访问目录内容。标准 registry 目录不包含位于 Cargo 父目录中的凭据和配置文件。

复用的是**依赖源码/索引数据，而不是主机编译产物或共享库**。Rust 和原生 C 依赖仍在 CentOS 用户态中编译、链接；已有编译产物的复用仅限构建器自身的 Cargo target 缓存。构建器缓存齐全后，也可以单独使用 `--offline` 而不传 registry 参数。缓存不完整时 Cargo 会失败，不会偷偷访问网络。

`--offline` **只作用于 Cargo**。容器镜像、Dockerfile frontend、Vault RPM、Rust 归档、pnpm 和前端依赖仍需网络访问，除非对应构建层或下载结果已经缓存。`--image-only` 不使用 registry 种子，也不执行 Cargo，因此不能据此认定 Cargo 已具备离线构建条件。这些缓存选项不代表产物已在目标 VPS 运行验证通过。

完整构建成功后，输出目录包含：

| 产物 | 内容 |
| --- | --- |
| `nyro-server` | 已 strip 的 GNU/Linux x86_64 原生 release 可执行文件，内嵌 WebUI。 |
| `SHA256SUMS` | 可执行文件的 SHA-256 校验值。 |
| `build-info.txt` | 构建环境 RPM/编译器版本及可执行文件的 ELF 动态信息。 |

这些文件是构建产物，不是已配置好的部署。构建失败时，复用的输出目录可能保留旧文件；必须检查构建退出状态，区分不同构建时建议使用新目录。

## 固定版本与检查内容

[CentOS 构建 Dockerfile](<../../deploy/centos7/Dockerfile>) 是镜像摘要、Rust 归档校验值、包版本断言和构建参数的准确信息来源：

- CentOS `7.6.1810` 基础镜像固定 digest；仅启用 Vault 的 `7.6.1810/os/x86_64` 仓库，不执行升级到较新 CentOS 用户态的 `yum update`。
- 断言 `centos-release-7-6.1810.2`、glibc `2.17-260`、libgcc/libstdc++ `4.8.5-36`；完整 RPM 标识见 Dockerfile。原生编译使用 CentOS GCC 工具链和开发头文件。
- 官方 Rust/Cargo `1.96.0`，分别校验 `x86_64-unknown-linux-gnu` 的 `rustc`、`cargo`、`rust-std` 归档 SHA-256 后安装（不下载未使用的文档和可选工具）。不能仅因 edition 2024 就认为 Rust 1.85 足够：当前锁定的服务端依赖包含要求 Rust 1.88 的 `home 0.5.12`，源码也使用 let-chains。
- 使用通用 `x86-64` CPU 目标、GCC 链接驱动和 `-C linker-features=-lld`，交由 CentOS 链接器链接。需要跨机器部署时，不要改成 `target-cpu=native`。
- 前端在独立、固定 digest 的 Node `22.22.0` 镜像中通过 pnpm `11.0.0` 和冻结锁文件构建。只有生成的 WebUI 静态资源进入 CentOS 源码构建；现代前端环境不参与服务端原生链接。
- 执行 `cargo build --locked -p nyro-server --release --target x86_64-unknown-linux-gnu`。保留默认的内嵌 WebUI 特性，不构建桌面/Tauri 二进制。

选中的原生依赖包括 ring、内置 SQLite 和 Zstd。[二进制验证脚本](<../../deploy/centos7/verify-binary.sh>) 检查 x86_64 架构、运行时动态依赖白名单、所需 GLIBC 符号版本不高于 2.17，并在构建容器中检查动态加载及 `--version`。应查看实际构建输出，不能假定这些检查已经通过。

### 构建用证书不等于运行时 CA 包

Node 基础镜像中的较新证书包被复制到构建器，用于构建时下载。它**不替换 CentOS 共享库**，**不会作为服务端运行时信任库内嵌**，也**不会随三个产物导出**。VPS 上游 TLS 连接仍需要有效且经过适当维护的 CA 信任配置。不要通过关闭 TLS 校验绕过根证书过期问题。

### 用户态不等于内核

Docker 使用构建主机的内核。容器复现的是旧用户态库，**不是 Linux `3.10.0-957` 内核**。ELF 检查和容器内执行，无法证明所有运行时系统调用及网络路径都能在目标 VPS 上正常工作。正式部署前，必须在真实目标机器上验证原生可执行文件。本方案也不会让普通 Ubuntu 环境构建的发布二进制自动兼容 CentOS 7。

## 复制产物并进行非服务化冒烟检查

以下通用示例通过 SSH 将原生产物传到目标机的**新建专用目录**。替换 `user@example-vps` 和示例目录，不要覆盖正在运行的生产二进制。

```bash
# 构建主机：完整构建成功后执行。
(cd target/centos7 && sha256sum -c SHA256SUMS)
ssh user@example-vps 'mkdir -m 700 "$HOME/nyro-centos7-check"'
scp target/centos7/{nyro-server,SHA256SUMS,build-info.txt} \
  user@example-vps:nyro-centos7-check/
ssh user@example-vps

# 以下命令在 VPS 中执行。
cd "$HOME/nyro-centos7-check"
sha256sum -c SHA256SUMS
chmod 700 nyro-server
./nyro-server --version
./nyro-server --help
```

`--version` 和 `--help` 会在初始化网关前退出。自动检查可将 [smoke.sh](<../../deploy/centos7/smoke.sh>) 复制到 VPS，执行 `bash smoke.sh /absolute/path/to/nyro-server`。脚本确认 29530/29531 端口未占用，创建隔离的临时数据和 HOME，检查健康/就绪接口及内嵌 WebUI，再停止临时进程（90 秒超时，随后最多 5 秒终止宽限期）。临时数据和日志保留在输出的路径供检查。

若需手动检查 HTTP，先确认所选端口未占用，再以**全新临时数据目录**前台启动，不使用生产数据。以下命令清理继承的环境，避免意外使用 `NYRO_*` 配置或数据库 DSN：

```bash
scratch=$(mktemp -d /tmp/nyro-centos7-smoke.XXXXXX)
env -i HOME="$HOME" PATH=/usr/bin:/bin ./nyro-server \
  --mode all --storage-backend sqlite --data-dir "$scratch" \
  --proxy-host 127.0.0.1 --proxy-port 29530 \
  --admin-host 127.0.0.1 --admin-port 29531
```

在另一个连接到 VPS 的 SSH 会话中执行：

```bash
curl --fail --silent --show-error --max-time 5 http://127.0.0.1:29530/health
curl --fail --silent --show-error --max-time 5 http://127.0.0.1:29530/readyz
curl --fail --silent --show-error --max-time 5 http://127.0.0.1:29531/healthz
curl --fail --silent --show-error --max-time 5 http://127.0.0.1:29531/readyz
```

正常响应为 HTTP 200 和 `{"status":"ok"}`。就绪检查可能返回 HTTP 503，以及 `schema_pending` 或 `unavailable`。这些无需鉴权的探针不调用模型推理，但**服务端启动并非只读操作**：会初始化临时存储，并可能下载、写入模型目录缓存；若存在已配置的 provider，还会产生后台用量/OAuth 活动。清理环境可能移除自定义 CA 配置，如有需要应显式配置可信证书。

用 Ctrl-C 停止前台进程。保留日志和临时目录路径供检查；不再需要时，只清理已确认可丢弃的文件。此检查不需要开放防火墙、配置 systemd、使用生产凭据或在 VPS 安装 Rust。生产部署、鉴权、持久化、运行时 CA 维护及进程管理是独立步骤，详见[服务端配置](<README.md>)。
