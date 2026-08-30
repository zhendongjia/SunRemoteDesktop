# SunRemoteDesktop

SunRemoteDesktop 是一个面向长期扩展的桌面共享项目：使用 SunRDP 作为 RDP 传输和画面更新核心，把远程客户端连接到“当前本地桌面”，而不是创建一个独立的 Windows 登录会话。

当前阶段先实现 Windows，代码结构为后续 Linux（X11/Wayland）和 macOS 保留平台抽象。管理界面使用跨平台 GUI，SunRDP 服务、权限模型、会话桥协议和配置格式位于共享核心中。

## 当前能力

- Windows 系统服务与由服务管理的物理控制台代理分离运行；不要求用户先登录，也不依赖登录启动项。
- 控制台代理只附着到 `WTSGetActiveConsoleSessionId` 对应的物理控制台，并跟随 `default`、`winlogon` 和 `screensaver` 输入桌面切换；捕获和键鼠操作会同时呈现在本地显示器上。
- 版本化本地命名管道桥；画面拥塞时只保留最新帧，输入走独立反向通道。
- SunRDP 服务端核心，可接受标准 RDP 客户端连接。
- TLS 传输和本地账户密码验证。
- 登录页可以在首次成功验证后记住当前客户端；后续从同一 Tailscale 节点连接时直接复用已获准的账户，不保存密码。非 Tailscale 连接只能按来源地址识别，界面会明确提示这一边界。
- TLS 内的认证界面支持定时重绘，不依赖桌面持续出帧；认证前不会发送真实桌面或转发键鼠到 Windows。
- 登录前界面直接采用 RDP 客户端请求的桌面尺寸，并支持键盘、绝对/相对鼠标和 MS-RDPEI 单指触摸；认证后如果客户端与共享屏幕尺寸不同，会先让用户选择缩放到客户端画布或修改共享会话的主显示器分辨率。如果已有客户端占用物理控制台，后来登录的客户端会在同一页看到“断开其他客户端”选项，明确确认后才会接管。
- 会话代理可在 Windows 显示模式变化后继续传送新尺寸；缩放模式保持桌面长宽比并居中留边，绝对鼠标坐标按实际画面区域同步换算。
- 支持 RDP Display Control 动态分辨率：客户端窗口变化会重建 RDP 画布。用户在认证后选“缩放显示”时不会自动改动物理屏；选“匹配显示器”时会优先切换到总像素数不超过客户端画布、且宽高比和尺寸最接近的临时硬件显示模式，避免高分辨率物理桌面缩小后造成字体过小；控制权在接管时转移，最后一个拥有控制权的 RDP 客户端断开时恢复首个连接前的本地显示模式。
- 分辨率选择页可以独立记住“缩放显示”或“匹配显示器”；同一客户端下次连接会自动应用。记住客户端和分辨率都不会绕过已有会话的接管确认，后来连接仍须明确勾选“断开其他客户端”。
- Windows 发布配置当前只广告 NSCodec，以兼容会接收 standalone RemoteFX 更新却保持黑屏的 Windows App 版本；不支持 NSCodec 的客户端退回标准位图更新。RemoteFX 编码实现和测试保留，待按客户端能力建立可靠的自适应选择后再启用。
- 本地账户白名单、端口、帧率、最大连接数和是否允许控制的界面配置。
- `run`、`agent`、`admin`、`service` 四种公开运行入口，以及只供服务内部使用的隐藏 `console-agent` 入口。
- 独立的平台接口，后续可以增加 X11、Wayland 和 macOS 实现，不需要改动 SunRDP 核心。

## 重要的 Windows 运行边界

Windows 服务会在开机时自动启动，但服务运行在 Session 0，不能直接捕获物理控制台桌面。因此当前版本由两部分协作：

1. 系统服务：负责运行 SunRDP、TLS、本地账户验证、权限和连接生命周期。
2. 物理控制台代理：由服务使用 LocalSystem 令牌启动到当前物理控制台及其当前输入桌面，负责捕获真实桌面并注入键鼠。

服务在没有代理时也会立即监听 RDP，并按客户端尺寸显示 SunRDP 受保护访问页；认证后如果控制台捕获代理尚不可用，则停留在等待控制台页面，仍不会发送真实桌面帧或转发输入。服务端只接受由自身启动、令牌身份为 LocalSystem 且 Windows Session ID 等于当前物理控制台的代理，并在控制台 Session 或输入桌面切换时替换旧代理；普通 Windows RDP 登录会话里的代理会被拒绝，避免把另一个远程会话误当成本地桌面。服务不会尝试捕获 Session 0。

控制台无人登录、已锁定或显示安全桌面时，代理会跟随 Windows 的 `winlogon`/`screensaver` 输入桌面，因此远端看到的是和物理屏一致的系统登录或锁屏界面，并可使用 Windows 自己的凭据流程登录。SunRemoteDesktop 不实现、替换或绕过 Windows Credential Provider，也不降低安全桌面策略；需要安全注意序列的系统配置仍受 Windows 对软件注入的限制。`run` 和公开的 `agent` 仅保留为已登录会话中的开发调试入口，不参与正式服务部署。

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

单独运行普通会话代理（仅供开发调试，正式服务不会使用）：

```bash
target/release/sun-remote-desktop.exe agent
```

配置文件默认位于：

```text
C:\ProgramData\SunRemoteDesktop\config.toml
```

默认监听 `3390` 端口，以避开 Windows 自带远程桌面的 3389。客户端地址写作 `主机名:3390`。

认证界面可以用鼠标或客户端的直接触摸模式选择输入框、按钮和“记住此客户端”，也可以使用 `Tab` 切换、`Space` 勾选、`Enter` 提交。勾选后只有本次账户密码验证成功才会保存信任，而且只保存账户名和客户端身份，不保存密码。当前原生触摸支持单指点击和拖动，多点手势暂不转发。

分辨率选择页默认选中不改动本地显示器的“缩放显示”；方向键、鼠标或单指触摸可切换策略和“记住此显示选择”，`Space` 切换当前复选项。已有客户端在线时，还需勾选“断开其他客户端”再按 `Enter`，服务才会断开旧会话并让新客户端接管。已记住的显示策略只在客户端取得控制权后自动应用，绝不会自动踢出其他客户端。

受信任客户端记录位于受管理员 ACL 保护的 `C:\ProgramData\SunRemoteDesktop\Maintenance\trusted-clients.toml`。Tailscale 连接优先记录稳定节点 ID，因此 `jzd-mb0` 的虚拟地址变化不会要求重新认证；无法取得 Tailscale 身份时会退回来源 IP，登录页会显示相应提示。若要撤销全部已记住的登录和分辨率选择，请以管理员权限删除该文件后重启 SunRemoteDesktop 服务；文件缺失、损坏或账户已从 `allowed_users` 移除时，服务会安全地重新要求认证。
如果连接后黑屏或卡顿，请记录客户端名称/版本、是否已通过认证、黑屏后按 `Tab` 是否恢复。
监听端口和出现认证界面都不能替代认证后真实桌面及键鼠控制的端到端验证。
Windows 服务的运行日志写入 `C:\ProgramData\SunRemoteDesktop\sunrdp-service.log`，包括连接来源、协议错误和断开原因。

## 安装 Windows 服务

先构建 release 版本，再以管理员身份运行 `scripts/install-service.ps1`。首次安装的这一次管理员批准会同时注册开机自动启动服务、服务管理的物理控制台代理、入站规则和下述受限维护入口。无需用户登录或登录启动项。公网使用前应改用 VPN 或其他网络边界保护，并替换默认自签名证书。

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
- 校正服务自动启动、移除旧版会话代理登录启动项并修复防火墙规则；Domain/Private 网络允许入站连接，Public 网络只允许同一本地子网来源；检测到 Tailscale 虚拟网卡时，另建只绑定该网卡且只接受 Tailscale 地址段的规则；
- 每条 RDP TCP 连接启用 keepalive：空闲 30 秒后每 10 秒探测，连续 3 次失败即由系统回收半开连接，便于设备睡眠或网络切换后及时释放客户端槽位；
- 部署候选版本，等待端口恢复，失败时恢复旧服务路径。

此后正常部署、重启和权限修复不再请求 UAC：

```powershell
.\scripts\update-service.ps1 -CandidateBinary $candidate -ExpectedSha256 $sha256
.\scripts\invoke-maintenance.ps1 -Action Restart
.\scripts\invoke-maintenance.ps1 -Action RestartAgent
.\scripts\invoke-maintenance.ps1 -Action Repair
```

维护任务执行的是受保护目录中的固定脚本，只接受 `Deploy`、`Restart`、`RestartAgent` 和 `Repair`
四类结构化请求。部署候选必须位于当前项目的 `target` 目录、文件名必须为
`sun-remote-desktop.exe`，复制进受保护目录后还会重新校验 SHA256 和启动版本。
由于部署后的 Windows 服务以 LocalSystem 运行，能够控制本项目构建输出和维护请求的账户
应被视为拥有该服务的管理员级发布权限，不应把这些目录的写权限授予其他账户。
维护任务本身要求维护账户已经交互式登录；这一限制不影响 SunRemoteDesktop 服务的开机自动启动。
新服务启动后会从二进制内置副本刷新受保护的维护脚本，因此后续维护能力可以随服务升级而扩展，无需再次批准 UAC。新版维护脚本在 `Deploy` 时会同时替换服务管理的控制台代理；部署会重启系统服务并中断现有 SunRDP 连接。单独使用 `RestartAgent` 只切换控制台代理：新帧和系统输入转发会短暂停顿，但 SunRDP 传输连接与最后一帧在 3 秒交接宽限内保持；只有替代代理未及时恢复时才退回等待控制台页面。

本机认证界面的协议与性能探针（不输入凭据，仅发送 `Tab` 测试响应）：

```powershell
cargo run --example rdp_probe
cargo run --example rdp_probe -- --codec remotefx --resize 1000x700
```

探针仅连接本机，使用现有服务证书验证 TLS，不发送账户凭据。第一条命令验证发布配置实际使用的 NSCodec 认证首屏并报告编码 ID、首屏字节数和首个按键响应时间；第二条命令协商 RDP Display Control 和 RDPEI，发送真实动态布局请求，完成 Deactivation/Reactivation，并断言最终 RDP 画布、相对鼠标点击和直接触摸点击都能改变新认证画面。动态探针使用其客户端解码库支持的 RemoteFX；显示布局、输入通道与画面编码相互独立。探针不验证认证后的物理桌面捕获、显示质量或远程网络延迟。

认证后连接与真实桌面切换可用可选凭据模式复测。密码只从当前探针进程继承的环境变量读取，不作为命令行参数；应使用专门的一次性本地测试账户，并在测试后立即清除环境变量和账户：

```powershell
$env:SUNRDP_PROBE_USERNAME = 'SunRdpE2E'
$env:SUNRDP_PROBE_PASSWORD = Read-Host 'Temporary test-account password' -MaskInput
cargo run --example rdp_probe -- --authenticate --desktop 1000x700
cargo run --example rdp_probe -- --authenticate --takeover --desktop 1000x700
Remove-Item Env:SUNRDP_PROBE_USERNAME,Env:SUNRDP_PROBE_PASSWORD
```

认证探针要求所选客户端画布与物理屏尺寸不同；它在 Windows 验证凭据后确认默认“缩放显示”选项，并断言访问页切换为真实桌面时连接仍然存活。已有另一探针或真实客户端在线时，`--takeover` 会额外确认“断开其他客户端”复选项。加上 `--post-auth-wait-seconds 8` 可在认证后持续消费桌面 Surface 更新，适合同时执行 `scripts/invoke-maintenance.ps1 -Action RestartAgent`，复测控制台代理交接期间 RDP 连接是否保持。它仍不能替代远程设备上的显示质量、网络延迟和本地可见输入检查。

## 设计文档

详细的模块边界、Windows 会话代理设计以及 Linux/macOS 移植路线见 [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)。
功能阶段和后续扩展清单见 [`docs/ROADMAP.md`](docs/ROADMAP.md)。

## SunRDP 与第三方依赖

SunRDP 是 SunRemoteDesktop 自有的 RDP 服务组件名称。当前 SunRDP 实现直接使用第三方开源项目 [IronRDP](https://github.com/Devolutions/IronRDP) 的 `ironrdp-server` crate，负责标准 RDP 协议编解码、连接状态机以及显示和输入处理接口。该依赖不会以 IronRDP 的名称作为产品服务对外展示。

所用版本、用途和许可证见 [`THIRD_PARTY.md`](THIRD_PARTY.md)。

## 许可证

MIT OR Apache-2.0
