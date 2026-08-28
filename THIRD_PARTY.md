# Third-party software

SunRemoteDesktop 的产品组件使用 **SunRDP** 名称。SunRDP 当前基于下列第三方开源组件实现标准 RDP 协议能力；第三方项目名称不会替代 SunRDP 的产品名称。

## IronRDP

- 项目：[Devolutions/IronRDP](https://github.com/Devolutions/IronRDP)
- 直接依赖：`ironrdp-server` 0.13.x（当前锁定版本见 `Cargo.lock`）
- 用途：RDP 协议编解码、连接状态机、TLS 接入，以及显示和输入处理接口
- 许可证：MIT OR Apache-2.0

`ironrdp-server` 会引入同一项目中的若干传递依赖，实际版本以 `Cargo.lock` 为准。分发二进制或源代码时应保留适用的许可证文本和第三方版权声明。

## 其他 Rust 依赖

其他直接与传递依赖及其精确版本记录在 `Cargo.toml` 和 `Cargo.lock` 中。发布流程应生成完整的软件物料清单和许可证报告，不能仅以本文件替代自动化合规检查。
