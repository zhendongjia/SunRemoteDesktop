# RdpDesktopHost

RdpDesktopHost 是一个面向长期扩展的桌面共享项目：使用 RDP 作为传输和画面更新协议，把远程客户端连接到“当前本地桌面”，而不是创建一个独立的 Windows 登录会话。

当前阶段先实现 Windows，代码结构为后续 Linux（X11/Wayland）和 macOS 保留平台抽象。管理界面使用跨平台 GUI，RDP 服务、权限模型和配置格式位于共享核心中。

## 当前能力

- Windows 交互式会话下的桌面捕获和键鼠注入原型。
- 基于 IronRDP 的 RDP 服务端骨架。
- TLS 传输和本地账户密码验证。
- 本地账户白名单、端口、帧率、最大连接数和是否允许控制的界面配置。
- `run`、`admin`、`service` 三种运行入口。
- 独立的平台接口，后续可以增加 X11、Wayland 和 macOS 实现，不需要改动 RDP 核心。

## 重要的 Windows 运行边界

Windows 服务可以在开机、用户尚未登录时启动监听端口，但服务运行在 Session 0，不能直接把登录用户的交互式桌面当作自己的桌面捕获。要实现“开机后自动运行且未登录也能看到登录前/登录后的真实桌面”，正式版本需要两部分协作：

1. 系统服务：负责监听 RDP、TLS、本地账户验证、权限和连接生命周期。
2. 交互式会话代理：由系统在用户会话建立时启动，负责捕获真实桌面并把键鼠事件交给该会话。

当前代码已经把这两个职责分开到服务入口和平台抽象中；`service` 入口用于后续接入会话代理，`run` 适合先在已登录的 Windows 会话中验证协议链路。不要把当前原型直接当作生产级“未登录远程控制”部署。

## 构建

在 Windows 的 MSYS2 UCRT64 环境中：

```bash
cargo check
cargo build --release
```

首次启动管理界面：

```bash
target/release/rdp-desktop-host.exe admin
```

运行当前交互式会话版本：

```bash
target/release/rdp-desktop-host.exe run
```

配置文件默认位于：

```text
C:\ProgramData\RdpDesktopHost\config.toml
```

## 安装 Windows 服务

先构建 release 版本，再以管理员身份运行 `scripts/install-service.ps1`。安装脚本会注册自动启动服务并添加私有网络入站规则。公网使用前应改用 VPN 或其他网络边界保护，并替换默认自签名证书。

卸载使用 `scripts/uninstall-service.ps1`。

## 设计文档

详细的模块边界、Windows 会话代理设计以及 Linux/macOS 移植路线见 [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)。
功能阶段和后续扩展清单见 [`docs/ROADMAP.md`](docs/ROADMAP.md)。

## 许可证

MIT OR Apache-2.0
