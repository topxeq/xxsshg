# xxsshg

xxssh 的 GUI 版本：轻量级图形 SSH 客户端，与 [xxssh](../repo) **共用同一份配置文件**。

- 单文件运行，无 WebView / Electron / 运行时依赖（Windows 静态 CRT，release 约 14MB）
- 纯 Rust：egui/eframe（GUI）+ alacritty_terminal（VT 终端模拟）+ russh（SSH，与 xxssh 同版 0.62/ring）
- 全功能终端：256 色/truecolor、vim/htop 等全屏程序、CJK 双宽字符、回滚缓冲、选中复制、括号粘贴
- 保活与 xxssh 一致：每 20s 心跳、3 次无响应判死
- 认证：密码 / 私钥（含加密私钥）；未存密码时弹窗输入；首次连接弹指纹确认
- SOCKS5 代理：服务器独立代理 > 全局代理（合并规则与 xxssh 相同）
- 多标签终端、中/英/繁界面、深/浅主题

## 配置文件（与 xxssh 互通）

| 文件 | 归属 | 说明 |
|------|------|------|
| `~/.xxssh/servers.json` | 与 xxssh 共用 | xxsshg 直接读写，密码 TXDEF 加密格式与 xxssh 逐字节兼容 |
| `~/.xxssh/settings.json` | 与 xxssh 共用 | 界面语言、全局代理等 |
| `~/.xxssh/gui.json` | xxsshg 独有 | 窗口/主题/字号/回滚行数/铃声等 GUI 设置，xxssh 不感知 |

环境变量 `XXSSH_CONFIG`、`XXSSH_GUI_CONFIG` 可分别覆盖路径。两边保存的服务器互相可见、可连——servers.json 的 schema 与 TXDEF 格式是两个程序之间的兼容契约（见 `src/xconfig.rs` 的契约测试，改动需同步 xxssh）。

## 构建与运行

```bash
cargo build --release
# 产物：target/release/xxsshg.exe（单文件）
cargo test    # 26 个测试，含与 xxssh 的格式契约测试
```

## 使用

1. 启动后左侧自动加载 xxssh 已保存的服务器列表
2. 单击选中，双击或点 Connect 连接；New/Edit/Delete 管理服务器
3. 终端内：拖选复制（可在设置关闭）、`Ctrl+Shift+C/V` 复制粘贴、`Ctrl+滚轮` 无、滚轮回滚历史（alt-screen 程序自动转方向键）
4. Settings 中可切换语言/主题/字号/回滚行数/铃声，保存写入 gui.json 与 settings.json

## 代码结构

```
src/
├── main.rs     入口：加载配置、后台 tokio runtime、CJK 系统字体回退、图标
├── app.rs      主界面：侧栏服务器列表 + 标签页 + 各类弹窗
├── term.rs     终端控件：alacritty_terminal 模型 -> egui 渲染、键盘/IME 编码、选区
├── session.rs  SSH 会话（连接/认证/保活/PTY），通过 channel 与 UI 解耦
├── xconfig.rs  servers.json/settings.json 读写（格式契约 + 契约测试）
├── gconfig.rs  gui.json 读写
├── i18n.rs     中/英/繁翻译（Language serde 格式与 xxssh 一致）
└── txdef.rs    TXDEF 密码加密（与 xxssh 逐字节一致，含官方测试向量）
```

本目录是独立 git 仓库，不依赖 xxssh 源码；维护策略为"复制 + 契约测试锁定格式"。
