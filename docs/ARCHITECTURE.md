# Architecture

## 目标

项目的核心目标是“镜像当前桌面”，不是让 RDP 协议创建第二个登录会话：

```text
RDP client
    │  RDP/TLS
    ▼
RDP server ── auth/policy ── local session bridge ── platform capture/input
    │                                         ├─ Windows
    │                                         ├─ Linux X11
    │                                         ├─ Linux Wayland
    │                                         └─ macOS
    ▼
same physical desktop
```

RDP 只负责传输、桌面更新和输入事件。桌面捕获、输入注入、本地账户验证和开机启动属于平台适配层或平台服务层，不能把 Windows 的实现细节泄漏进协议核心。

## 模块边界

- `config`：稳定的 TOML 配置和权限白名单。
- `auth`：协议凭据到平台本地账户验证的适配器。
- `display`：把平台帧转换成 IronRDP 的桌面更新，并负责多客户端读取。
- `input`：把 RDP 键盘/鼠标事件交给当前桌面会话。
- `bridge`：定义版本化的服务—会话代理协议，以及各平台的 IPC 实现。
- `host`：组合认证、连接数限制、TLS、显示和输入处理器。
- `platform`：桌面捕获、输入注入和会话生命周期的跨平台接口。
- `admin`：只编辑配置，不直接承担协议或平台逻辑。
- `service`：操作系统服务入口；Windows 服务和用户会话代理通过本地 IPC 连接。

## Windows 两进程模型

当前 Windows 实现使用下列模型：

### 系统服务

- `LocalSystem` 或专用低权限服务账户运行。
- 开机启动，等待会话代理后监听 TCP 端口。
- 读取 `C:\ProgramData\SunRemoteDesktop\config.toml`。
- 用 `LogonUser` 验证本地账户密码，再应用白名单。
- 管理 TLS 私钥、连接数、审计日志和会话代理连接。
- 通过受 ACL 保护的版本化命名管道与会话代理通讯。

### 交互式会话代理

- 由系统登录自启动项在用户会话建立时启动。
- 在该用户 Session 中调用桌面捕获 API。
- 接收服务转发的键鼠事件并调用 `SendInput` 或更细粒度的输入 API。
- 将帧通过命名管道/共享内存送回服务。
- 用户注销后主动断开，服务继续运行并等待下一次会话。

这样服务不会错误地把 Session 0 当作用户桌面，也能让远程客户端继续使用标准 RDP 客户端连接服务端。

### 会话桥协议 v1

- 代理连接后先发送魔数、协议版本、桌面尺寸和能力标志。
- 代理到服务方向传送带尺寸校验的紧密 RGBA 帧；单帧上限为 128 MiB。
- 服务到代理方向传送有类型的键盘和鼠标事件。
- 代理端帧源使用 latest-frame 语义。管道写入变慢时最多保留正在发送的帧和最新帧，不会按帧率无限积压内存。
- 命名管道拒绝远程客户端，使用 first-instance 防抢占，并通过 DACL 仅允许 LocalSystem、管理员和本机交互式用户连接。
- 代理断开后服务保留 RDP 核心并等待同尺寸代理重连；桌面尺寸发生变化时需要重启服务以重新协商 RDP 桌面。

当前一次只选择一个交互式会话代理。会话切换、登录/注销事件跟踪、锁定状态展示和管理员可选择目标会话将在后续阶段实现。

## 平台适配路线

### Windows

当前使用 `xcap` 做桌面捕获，使用 Win32 `SendInput` 注入键鼠，使用 `LogonUser` 做本地凭据验证。捕获和输入注入只存在于交互式会话代理，系统服务不访问 Session 0 桌面。

### Linux X11

捕获优先考虑 XComposite/SHM 或成熟的 PipeWire 桥接；输入优先使用 XTest。必须识别 DISPLAY、Xauthority 和当前 seat，不能把服务账户环境变量当作用户桌面环境。

### Linux Wayland

Wayland 不允许后台程序无条件读取桌面或注入输入。需要通过 xdg-desktop-portal ScreenCast、PipeWire 和 compositor/desktop-specific input portal 获取用户授权。Wayland 适配应作为独立后端，并在 GUI 中明确显示授权状态。

### macOS

使用 ScreenCaptureKit 或系统屏幕录制接口；输入注入需要 Accessibility 权限。服务/登录项模型要遵循 launchd 和用户登录会话边界。

## 协议与安全

- RDP 监听默认绑定 `0.0.0.0:3389`，部署时应优先限制到私有网络或 VPN。
- 当前原型自动生成自签名 TLS 证书，生产版应支持证书导入、指纹校验或企业证书配置。
- 本地账户白名单为空时拒绝所有连接。
- 远程控制可单独关闭，形成只读桌面共享。
- 后续应增加连接审计、速率限制、剪贴板/文件传输的显式开关，以及会话代理 IPC 的身份校验。
- 当前命名管道 DACL 限制了可连接的本地令牌范围，但尚未把代理进程身份绑定到管理界面的远程访问白名单；这是后续安全加固项。
- Windows 登录界面、锁屏后的安全桌面和 UAC 安全桌面不在当前捕获范围内，项目不会通过降低系统安全策略来绕过该边界。
