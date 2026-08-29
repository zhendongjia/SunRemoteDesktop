# SunRemoteDesktop

SunRemoteDesktop 是一个面向长期扩展的桌面共享项目：使用 SunRDP 作为 RDP 传输和画面更新核心，把远程客户端连接到“当前本地桌面”，而不是创建一个独立的 Windows 登录会话。

当前阶段先实现 Windows，代码结构为后续 Linux（X11/Wayland）和 macOS 保留平台抽象。管理界面使用跨平台 GUI，SunRDP 服务、权限模型、会话桥协议和配置格式位于共享核心中。

## 当前能力

- Windows 系统服务与交互式会话代理分离运行。
- 会话代理捕获当前本地桌面并注入键鼠，操作会同时呈现在本地显示器上。
- 版本化本地命名管道桥；画面拥塞时只保留最新帧，输入走独立反向通道。
- SunRDP 服务端核心，可接受标准 RDP 客户端连接。
- TLS 传输和本地账户密码验证。
- TLS 内的认证界面支持定时重绘，不依赖桌面持续出帧；认证前不会发送真实桌面或转发键鼠到 Windows。
- 通过 RemoteFX 或 NSCodec 压缩画面，保留仅提供 NSCodec 的客户端兼容路径；实际编码取决于客户端协商结果。
- 本地账户白名单、端口、帧率、最大连接数和是否允许控制的界面配置。
- `run`、`agent`、`admin`、`service` 四种运行入口。
- 独立的平台接口，后续可以增加 X11、Wayland 和 macOS 实现，不需要改动 SunRDP 核心。

## 重要的 Windows 运行边界

Windows 服务会在开机时自动启动，但服务运行在 Session 0，不能直接捕获登录用户的交互式桌面。因此当前版本由两部分协作：

1. 系统服务：负责运行 SunRDP、TLS、本地账户验证、权限和连接生命周期。
2. 交互式会话代理：在用户登录时自动启动，负责捕获真实桌面并把键鼠事件交给该会话。

服务在没有代理时保持运行并等待，不会尝试捕获 Session 0；首个代理连接后才开始监听 RDP。Windows 登录界面、锁屏后的安全桌面和 UAC 安全桌面仍不可见、不可控制。也就是说，当前版本能够“开机自动运行”，但仍需要至少一个 Windows 用户完成交互式登录，才能共享其真实桌面。`run` 保留为已登录会话中的单进程调试入口。

## 构建

在 Windows PowerShell 中使用 Rust stable MSVC 工具链：

```powershell
cargo check
cargo build --release
```

如果本机尚未安装 Rust，请先安装 rustup 和 Visual Studio Build Tools 的
Desktop development with C++ 工作负载；项目本身不依赖 MSYS2/UCRT64 运行环境。

首次启动管理界面：

```bash
target/release/sun-remote-desktop.exe admin
```

运行当前交互式会话版本：

```bash
target/release/sun-remote-desktop.exe run
```

单独运行会话代理（通常由安装脚本自动配置）：

```bash
target/release/sun-remote-desktop.exe agent
```

配置文件默认位于：

```text
C:\ProgramData\SunRemoteDesktop\config.toml
```

默认监听 `3390` 端口，以避开 Windows 自带远程桌面的 3389。客户端地址写作 `主机名:3390`。

认证界面使用 `Tab` 切换账号/密码、`Enter` 提交，目前不支持用鼠标选择输入框。
如果连接后黑屏或卡顿，请记录客户端名称/版本、是否已通过认证、黑屏后按 `Tab` 是否恢复。
监听端口和出现认证界面都不能替代认证后真实桌面及键鼠控制的端到端验证。

## 安装 Windows 服务

先构建 release 版本，再以管理员身份运行 `scripts/install-service.ps1`。首次安装的这一次管理员批准会同时注册开机自动启动服务、登录后自动启动会话代理、私有网络入站规则和下述受限维护入口。安装完成时也会直接启动当前用户的代理，无需注销。公网使用前应改用 VPN 或其他网络边界保护，并替换默认自签名证书。

卸载使用 `scripts/uninstall-service.ps1`。

已有服务建议一次性安装受限维护入口。先在普通 PowerShell 中完成预检：

```powershell
$candidate = Resolve-Path target\release\sun-remote-desktop.exe
$sha256 = (Get-FileHash -Algorithm SHA256 $candidate).Hash
.\scripts\install-maintenance.ps1 -CandidateBinary $candidate -ExpectedSha256 $sha256 -PreflightOnly
```

确认构建、测试和预检全部通过后，只需一次管理员批准，使用相同参数去掉
`-PreflightOnly`。这次执行会同时完成以下工作：

- 把版本化服务文件迁移到受保护的 `C:\Program Files\SunRemoteDesktop`；
- 注册仅限当前维护账户、仅在该账户交互式登录时可启动的最高权限计划任务；
- 校正服务自动启动、会话代理启动项和 Domain/Private 防火墙规则；
- 部署候选版本，等待端口恢复，失败时恢复旧服务路径。

此后正常部署、重启和权限修复不再请求 UAC：

```powershell
.\scripts\update-service.ps1 -CandidateBinary $candidate -ExpectedSha256 $sha256
.\scripts\invoke-maintenance.ps1 -Action Restart
.\scripts\invoke-maintenance.ps1 -Action Repair
```

维护任务执行的是受保护目录中的固定脚本，只接受 `Deploy`、`Restart` 和 `Repair`
三类结构化请求。部署候选必须位于当前项目的 `target` 目录、文件名必须为
`sun-remote-desktop.exe`，复制进受保护目录后还会重新校验 SHA256 和启动版本。
由于部署后的 Windows 服务以 LocalSystem 运行，能够控制本项目构建输出和维护请求的账户
应被视为拥有该服务的管理员级发布权限，不应把这些目录的写权限授予其他账户。
维护任务本身要求维护账户已经交互式登录；这一限制不影响 SunRemoteDesktop 服务的开机自动启动。
兼容的现有会话代理在部署时保持运行，新代理路径在下次交互式登录时生效。

本机认证界面的协议与性能探针（不输入凭据，仅发送 `Tab` 测试响应）：

```powershell
cargo run --example rdp_probe -- --codec nscodec
cargo run --example rdp_probe -- --codec remotefx
```

探针仅连接本机，使用现有服务证书验证 TLS，报告实际编码 ID、首屏字节数与首个按键响应时间。
它验证认证界面的传输，不验证实际桌面的捕获、显示质量或远程网络延迟。

## 设计文档

详细的模块边界、Windows 会话代理设计以及 Linux/macOS 移植路线见 [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)。
功能阶段和后续扩展清单见 [`docs/ROADMAP.md`](docs/ROADMAP.md)。

## SunRDP 与第三方依赖

SunRDP 是 SunRemoteDesktop 自有的 RDP 服务组件名称。当前 SunRDP 实现直接使用第三方开源项目 [IronRDP](https://github.com/Devolutions/IronRDP) 的 `ironrdp-server` crate，负责标准 RDP 协议编解码、连接状态机以及显示和输入处理接口。该依赖不会以 IronRDP 的名称作为产品服务对外展示。

所用版本、用途和许可证见 [`THIRD_PARTY.md`](THIRD_PARTY.md)。

## 许可证

MIT OR Apache-2.0
