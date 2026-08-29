# Third-party software

SunRemoteDesktop 的产品组件使用 **SunRDP** 名称。SunRDP 当前基于下列第三方开源组件实现标准 RDP 协议能力；第三方项目名称不会替代 SunRDP 的产品名称。

## IronRDP

- 项目：[Devolutions/IronRDP](https://github.com/Devolutions/IronRDP)
- 直接依赖：`ironrdp-server` 0.13.x、`ironrdp-pdu` 0.9.x、`ironrdp-displaycontrol` 0.8.x（当前锁定版本见 `Cargo.lock`）
- 上游固定版本：所有直接使用的 IronRDP 工作区 crate 临时固定到提交 [`aa260fe`](https://github.com/Devolutions/IronRDP/commit/aa260fef9ebbc0a420b17e796616d710fb41e702)。该版本包含按块长度处理 Windows App 新 GCC 扩展的兼容修复、携带坐标的鼠标按钮事件，以及服务端 MS-RDPEI 动态虚拟通道接入；对应改动进入一致的 crates.io 发布版并完成兼容验证后再移除此固定提交。
- 用途：RDP 协议编解码、连接状态机、TLS 接入、显示和输入接口、Display Control 与 MS-RDPEI 动态虚拟通道，以及 RemoteFX/NSCodec 压缩能力协商。`ironrdp-displaycontrol` 负责解析客户端的单/多显示器布局请求；当前 SunRDP 只接受单主显示器动态布局。`ironrdp-rdpei` 由 `ironrdp-server` 间接引入，用于接收客户端原生触摸帧；SunRDP 当前只映射一个主触点。`ironrdp-server` 的 `__bench` 功能仅在测试中启用，用于检查重复重绘是否实际编码、以及编码后的字节数，不用于发布构建。
- 许可证：MIT OR Apache-2.0

`ironrdp-server` 会引入同一项目中的若干传递依赖，实际版本以 `Cargo.lock` 为准。分发二进制或源代码时应保留适用的许可证文本和第三方版权声明。

其中 `ironrdp-nscodec` 0.2.x 用于 NSCodec 位图压缩，兼容未提供 RemoteFX 的客户端；许可证同为 MIT OR Apache-2.0。

开发探针还直接使用 `ironrdp-async` 0.10.x、`ironrdp-connector` 0.10.x、`ironrdp-core` 0.2.x、`ironrdp-dvc` 0.8.x、`ironrdp-graphics` 0.9.x、`ironrdp-rdpei` 0.1.x、`ironrdp-session` 0.11.x 和 `ironrdp-tokio` 0.10.x。默认探针验证仅限本机的 TLS/RDP 连接、生产 NSCodec 认证首屏、帧解析和按键响应；动态尺寸探针通过 `drdynvc`/Display Control 发送真实显示布局请求，驱动 Deactivation/Reactivation，解码新尺寸下的 RemoteFX 认证画面，再通过相对鼠标和 RDPEI 分别点击访问页控件。许可证均为 MIT OR Apache-2.0。探针不发送账户凭据，证书验证使用明确指定的本机服务证书；这些开发依赖不额外加入发布程序。

## Fontdue

- 项目：[mooman219/fontdue](https://github.com/mooman219/fontdue)
- 直接依赖：`fontdue` 0.9.x（当前锁定版本见 `Cargo.lock`）
- 用途：在 SunRDP 登录前访问界面中栅格化抗锯齿文字。Windows 构建运行时读取系统已安装的 Segoe UI 或 Arial 字体；SunRemoteDesktop 不复制、嵌入或再分发这些 Windows 字体文件。系统字体不可用时回退到 `font8x8`。
- 许可证：MIT OR Apache-2.0 OR Zlib

## fast_image_resize

- 项目：[cykooz/fast_image_resize](https://github.com/cykooz/fast_image_resize)
- 直接依赖：`fast_image_resize` 5.x（当前锁定版本见 `Cargo.lock`）
- 用途：用户选择“缩放显示”时，使用 SIMD 优化的双线性插值把物理桌面帧等比例缩放到 RDP 客户端画布中的居中视口。
- 许可证：MIT OR Apache-2.0

## 其他 Rust 依赖

其他直接与传递依赖及其精确版本记录在 `Cargo.toml` 和 `Cargo.lock` 中。发布流程应生成完整的软件物料清单和许可证报告，不能仅以本文件替代自动化合规检查。
