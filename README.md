# SunRemoteDesktop

SunRemoteDesktop 是一个面向长期扩展的桌面共享项目：使用 RDP 作为传输和画面更新协议，把远程客户端连接到“当前本地桌面”，而不是创建一个独立的 Windows 登录会话。

当前阶段先实现 Windows，代码结构为后续 Linux（X11/Wayland）和 macOS 保留平台抽象。管理界面使用跨平台 GUI，RDP 服务、权限模型、会话桥协议和配置格式位于共享核心中。

## 当前能力

- Windows 系统服务与交互式会话代理分离运行。
- 会话代理捕获当前本地桌面并注入键鼠，操作会同时呈现在本地显示器上。
- 版本化本地命名管道桥；画面拥塞时只保留最新帧，输入走独立反向通道。
- 基于 IronRDP 的 RDP 服务端骨架。
- TLS 传输和本地账户密码验证。
- 本地账户白名单、端口、帧率、最大连接数和是否允许控制的界面配置。
- `run`、`agent`、`admin`、`service` 四种运行入口。
- 独立的平台接口，后续可以增加 X11、Wayland 和 macOS 实现，不需要改动 RDP 核心。

## 重要的 Windows 运行边界

Windows 服务会在开机时自动启动，但服务运行在 Session 0，不能直接捕获登录用户的交互式桌面。因此当前版本由两部分协作：

1. 系统服务：负责监听 RDP、TLS、本地账户验证、权限和连接生命周期。
2. 交互式会话代理：在用户登录时自动启动，负责捕获真实桌面并把键鼠事件交给该会话。

服务在没有代理时保持运行并等待，不会尝试捕获 Session 0；首个代理连接后才开始监听 RDP。Windows 登录界面、锁屏后的安全桌面和 UAC 安全桌面仍不可见、不可控制。也就是说，当前版本能够“开机自动运行”，但仍需要至少一个 Windows 用户完成交互式登录，才能共享其真实桌面。`run` 保留为已登录会话中的单进程调试入口。

## 构建

在 Windows 的 MSYS2 UCRT64 环境中：

```bash
cargo check
cargo build --release
```

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

## 安装 Windows 服务

先构建 release 版本，再以管理员身份运行 `scripts/install-service.ps1`。安装脚本会注册开机自动启动服务、登录后自动启动会话代理，并添加私有网络入站规则。安装完成时也会直接启动当前用户的代理，无需注销。公网使用前应改用 VPN 或其他网络边界保护，并替换默认自签名证书。

卸载使用 `scripts/uninstall-service.ps1`。

## 设计文档

详细的模块边界、Windows 会话代理设计以及 Linux/macOS 移植路线见 [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)。
功能阶段和后续扩展清单见 [`docs/ROADMAP.md`](docs/ROADMAP.md)。

## 许可证

MIT OR Apache-2.0
