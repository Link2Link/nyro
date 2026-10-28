# 在线更新（nyro-server）

[English](upgrade.md)

运行中的 `nyro-server` 可以在 WebUI 的 **设置 → 版本更新** 中一键安装新版本。版本只升不降，不提供回退。

## 工作原理

1. **检查**：实例从 GitHub 最新 Release 下载 `nyro-server-update.json`。这是一份用 minisign 签名的清单，列出各平台静态二进制的版本、说明、sha256 和大小。实例用编译进二进制的公钥（`crates/nyro-core/src/update/release.pub`）验签，未签名或被篡改的清单一律拒绝，公钥也无法在运行时修改。
2. **安装**：点击 **立即更新** 并确认后，实例依次执行：
   - 重新获取清单并验签；
   - 把本平台二进制下载到 `<data-dir>/upgrade/staging/`；
   - 按签名清单校验大小和 sha256；
   - 执行 `<新二进制> --version` 冒烟测试，提前发现架构不对、libc 缺失或 `noexec` 挂载等问题；
   - 把二进制移到 `<data-dir>/upgrade/versions/<version>/nyro-server`；
   - 在 `<data-dir>/upgrade/state.json` 里把它记为当前版本。
3. **重启**：监听器停止接收新连接，进行中的请求最多再等 30 秒，然后关闭数据库连接池，用相同的参数和环境变量 `exec` 新二进制。PID 不变，容器的 PID 1 不受影响。WebUI 等新版本响应后自动刷新页面。
4. **后续启动**：每次启动时，`nyro-server` 在绑定端口前先读取 `state.json`。如果已安装更新的版本，就切换过去；如果当前二进制版本相同或更新（例如镜像已重建为更新的版本），就清理已安装的更新，继续用当前二进制。

数据库迁移在新版本正常启动时执行。

```text
<data-dir>/upgrade/
  state.json                     {"schema":1,"active":"2.2.0","installed_at":...}
  versions/2.2.0/nyro-server     当前使用的二进制（旧版本会被清理）
  staging/                       下载中的临时文件
```

## 前提条件

- **Unix 主机**（Linux 或 macOS）。目前的发布产物覆盖 `linux-x86_64` 和 `linux-aarch64`，都是 musl 静态编译，不依赖 libc 版本，CentOS 7 也能运行。
- **可写且能持久保存的数据目录**。不满足时，WebUI 会提示无法在线更新。
- **开启 Admin 监听**（`--mode all` 或 `--mode admin`）。纯代理副本不做在线更新。
- **每个数据目录只跑一个实例**。共享存储的多副本部署请设置 `NYRO_SELF_UPDATE=false`，改用镜像滚动发布。
- **能访问 GitHub HTTPS**。访问不了时，可以在 **设置 → 代理配置** 中配置代理（更新请求会走网关代理），或者用 `NYRO_UPDATE_MANIFEST_URL` 指向镜像地址。安全性靠签名保证，与传输通道无关，镜像无法篡改实际安装的内容。

## Docker

已安装的更新保存在镜像的 `/var/lib/nyro` 数据卷里，`docker restart` 和 `docker compose up -d` 重建容器后依然生效。镜像里的二进制只负责引导启动。镜像本身需要先升级到包含此功能的版本，之后才能在线更新；更早的镜像需要先按常规方式升级一次。

- `docker inspect` 和镜像标签仍显示镜像的版本，**设置** 页显示的是实际运行的版本。
- 镜像重建为更新的版本后，已安装的更新会在下次启动时自动清理。
- 如果想强制改回镜像自带的二进制：停止容器，删除数据卷中的 `upgrade/` 目录。
- 请保留 `unless-stopped` 之类的重启策略。更新后进程如果退出，容器会以新版本重新启动。
- 重启期间健康检查可能会有几秒钟失败一次。

## 配置

| 参数 | 环境变量 | 默认值 | 作用 |
|------|----------|--------|------|
| `--update-check` | `NYRO_UPDATE_CHECK` | `true` | 允许检查新版本。设为 `false` 后不会发出任何更新相关的外部请求 |
| `--self-update` | `NYRO_SELF_UPDATE` | `true` | 允许在线安装。设为 `false` 时，WebUI 仍会提示有新版本 |
| `--update-manifest-url` | `NYRO_UPDATE_MANIFEST_URL` | GitHub 最新 Release 附件 | 签名清单地址（镜像） |

Admin API（都需要 admin token）：

| 路由 | 用途 |
|------|------|
| `GET /api/v1/system/version` | 当前运行版本、平台、是否支持在线更新（不支持时给出 `reason`） |
| `GET /api/v1/system/update/check[?force=true]` | 最新的签名版本，结果缓存 10 分钟，`force` 强制刷新 |
| `POST /api/v1/system/update/apply` `{"version":"X.Y.Z"}` | 开始安装。版本号必须等于最新版本，且比当前版本新 |
| `GET /api/v1/system/update/status` | 任务阶段（`downloading` / `verifying` / `installing` / `restarting` / `failed`）和进度 |

## 发布签名（维护者）

每次发布时，`.github/workflows/release.yml` 用仓库 Secret `NYRO_UPDATE_SIGNING_KEY` 签名，把静态二进制和 `nyro-server-update.json` 一起上传到 GitHub Release。

一次性配置：

```bash
minisign -G -W -p nyro-update.pub -s nyro-update.key   # 不加密的密钥，供 CI 使用
cp nyro-update.pub crates/nyro-core/src/update/release.pub
gh secret set NYRO_UPDATE_SIGNING_KEY < nyro-update.key
```

`nyro-update.key` 请离线保管。工作流每次都会用 `release.pub` 验证签名，对不上就让发布失败，所以 Secret 配错了也不会发出实例无法验证的清单。如果没有配置 Secret，发布仍会完成并给出警告，但运行中的实例收不到这个版本。

更换密钥时，需要先发布一个包含新 `release.pub` 的版本，并用**旧**密钥签名，因为旧版本实例只信任旧密钥。

`crates/nyro-core/tests/fixtures/update/` 下的测试数据用一次性密钥签名，可以用 `regenerate.sh` 重新生成。
