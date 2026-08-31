# xxsshg 开发进度记录

> 本文件记录开发会话进度，便于跨会话续接。最新在最前。
> 项目：xxsshg——xxssh 的 GUI 版（独立代码库 repo-gui，与 xxssh 共用 ~/.xxssh 配置）。
> 设计方案见上级目录 prd2-xxsshg.md。

---

## 2026-08-31 发布会话（v0.2.0）+ 本地终端/滚动条/热键/快速连接

### 发布
- v0.2.0 已发布到仙缘渡 **xxssh 卡片的 "Windows GUI" 平台**（与 xxdmg→xxdm 同模式，独立版本号）
- 卡片描述已加"Windows GUI 版"小节；旧 GUI v0.1.0 条目已删（每平台只留 latest）
- 发布脚本：xxssh 根目录 publish-xxsshg-X.Y.Z.ps1（PRODUCT_ID=xxssh, PLATFORM="Windows GUI"）
- ⚠️ 发布脚本 Verify 段落/结尾横幅是从 xxdmg 脚本复制的硬编码残留，输出会打印 xxdm 的版本列表——以 API 查询为准
- ⚠️ 批量删旧版本时 API 有传播延迟，删除后用 fresh GET 复核，误判"全删光"会虚惊

### 本会话功能与修复（0.1.0 → 0.2.0）
- **本地终端**：CMD/PowerShell/$SHELL 侧栏入口，portable-pty ConPTY → SessionHandle 适配（clink 自动注入生效）
- **快速连接**：⚡按钮 + 对话框（主机/端口/用户/密码，不写入 servers.json；记住主机/端口/用户，密码绝不落盘；回车提交）
- **滚动条**：终端右缘（拖动滑块/点击轨道），方向与主流终端一致（滑块底部=实时最新）
- **Home/End**：滚动到缓冲区头/尾（Shift+Home/End 发远端做行编辑；alt-screen 下直通远端）
- **热键**：Ctrl+W 关闭当前页签、Ctrl+N 新开 CMD 页签；应用级全局生效（输入层状态机边沿检测），设置里可自定义（格式 "Ctrl+W"，留空禁用）
- **UI**：Connect 行布局修正（⚡/☰ 固定槽位不再溢出）、☰ 与 ⚡ 等尺寸、退出确认框居中、快速连接回车提交
- **渲染**：BASELINE TOP 对齐（文字不再偏低）、首行 ascender 裁剪余量、CJK 基线按字体 hhea 度量精确补偿、build hash 修正（盯 refs/heads/main）
- 二进制补 `--version`（安装脚本版本检测依赖）

### 重要修复（详见 exp1.md）
- deferred_new_cmd 标志未复位 → 每帧无限开 CMD 页签（热键"不停地建"的真凶）
- 滚动条方向修复两次才落地（文本替换静默失败）——修完必须 grep 源码复核
- CJK/EN 基线对齐（skrifa 读 hhea 度量计算，随字号缩放）
- 选区高亮在逐格渲染重写时丢失 → 已补回

### 本地终端已知问题
- Windows 26200 预览版 ConPTY 输入注入回归：写入成功但按键不达子进程（微软已知问题类，WT 不受影响）——稳定版待验证
- 2026-08-30 会话中"输入不工作"的初始判断即此问题；后续用户实测输入正常，说明与 clink 时序相关而非普遍回归，保留观察

## 2026-08-30 开发+发布会话（v0.1.0）

### 触发：从零实现 xxsshg 并发布到仙缘渡 xxssh 卡片（Windows GUI 平台）

### 交付内容

**A. 项目骨架与技术栈**
- 独立 Cargo 项目 repo-gui（独立 git 仓库，不动 xxssh）
- egui/eframe 0.36（GUI）+ alacritty_terminal 0.26（VT 模拟）+ russh 0.62/ring（与 xxssh 同版）
- 依赖全部 MIT/Apache（arboard、skrifa、image、portable-pty 等），已扫描 442 个依赖确认无 GPL 传染

**B. 配置互通（格式契约）**
- xconfig.rs：读写 ~/.xxssh/servers.json + settings.json，TXDEF 加密逐字节兼容（txdef.rs 从 xxssh 0.5.2 复制，含官方测试向量）
- 契约测试：xxssh 写入的密文夹具解密、字段集合断言、语言 serde 格式断言
- gconfig.rs：GUI 独有 gui.json（窗口/主题/字号/回滚/铃声/滚轮方向/字体），随版本演进

**C. 会话层 session.rs**
- 事件式 API：spawn_connect → oneshot 结果 + ConnectRequest 通道（密码/口令/指纹询问）+ SessionHandle（input/resize/output/event/close）
- 保活 20s×3、PTY 模式 OPOST/ONLCR/CS8、EOF 3s 宽限、SOCKS5 合并（服务器>全局）——逻辑照搬 xxssh 已验证实现
- 主机指纹：持久 TOFU（~/.xxssh/known_hosts），接受变更密钥时清除旧条目（russh learn 只追加）
- CPR/DA 等终端查询由 alacritty 模型自动应答（EventProxy::PtyWrite → 输入通道）

**D. 终端控件 term.rs**
- 逐格绝对坐标渲染（每字符画在自己的格子坐标上——消除排版步进与格子的累积误差）
- 行高取 egui row_height（字体度量），TOP 基线对齐；CJK 按字体真实 hhea 度量计算基线补偿（fonts.rs compute_baseline_shift，skrifa 读取）
- IME：终端聚焦时 IMEAllowed(true)、IMERect 跟随光标；组词（Preedit）内联渲染在光标处，组词期间屏蔽原始按键；Commit 上屏发送
- Ctrl+C/X：egui-winit 拦截为 Copy/Cut 事件——按 Windows Terminal 约定处理（有选区=复制+清选区，无选区=发真实 ^C/^X）
- 终端聚焦时 EventFilter 声明独占 Tab/方向键/Esc（否则 egui 拿去做焦点导航）
- Ctrl+= / Ctrl+- / Ctrl+0 缩放字号（持久化 gui.json）
- 光标块覆盖字形 em box（非整行高）；调试覆盖层 XXSSHG_DEBUG_CURSOR=1

**E. 本地终端 local.rs**
- CMD / PowerShell / $SHELL 经 portable-pty（ConPTY）→ 适配成与 SSH 相同的 SessionHandle
- 侧栏"本地终端"区按钮，点击开标签页；clink（cmd AutoRun）自动注入生效
- ⚠️ Windows 26200（insider）ConPTY 输入注入回归：写入成功但按键不达子进程（微软/terminal 已知问题类，WT 不受影响）——详见 exp1.md

**F. 发布（进 xxssh 卡片，非独立产品）**
- 模式与 xxdmg→xxdm 相同：PRODUCT_ID=xxssh，PLATFORM="Windows GUI"，独立版本号 0.1.0
- publish-xxsshg-0.1.0.ps1（xxssh 根目录，凭据不入 git）；发布后卡片出现 "Windows GUI v0.1.0"
- 二进制补 --version（安装脚本版本检测依赖）
- 曾误建独立 xxsshg 产品 → deleteProduct + confirm=DELETE-PRODUCT 删除（见 exp1.md）

### 验证
- 31 个测试全绿（含与 xxssh 的格式契约测试、字节流回放测试、光标 resize 测试）
- 无头 E2E：XXSSHG_AUTOCONNECT=xhw 自动连接测试服务器（日志确认秒连）；XXSSHG_E2E=1/2 会话层 E2E 与打字模拟
- 发布后端点验证：产品 API、latest、install/xxsshg.ps1、downloads 均正常
- Windows 26200 上 GUI 手测由用户完成（连接/渲染/中文/光标多轮迭代修复）

### 已知问题 / 待办
- 本地终端键盘输入在本机 Windows 26200 预览版不工作（OS 回归，等微软修复；稳定版待验证）
- 铃声 Sound 模式未真正发声（等同静音）
- Linux/macOS 构建未做（GUI 的 musl 构建是风险项，需按 prd2 §7 评估）
- 安装脚本 install/xxsshg.ps1 未发布（GUI 从卡片直接下载，与 xxdmg 一致；如需要再补）
- servers.json 双程序并发写为 last-write-wins（backlog：mtime 变更检测）

---

## 环境/命令速查

- 构建：cargo build --release；测试：cargo test；E2E：XXSSHG_E2E=1（SSH）/ XXSSHG_E2E=2（打字模拟）/ XXSSHG_LOCAL_E2E=1（本地终端）
- 无头自动连接：XXSSHG_AUTOCONNECT=服务器名（TOFU 自动信任）
- 发布：改 Cargo.toml 版本 → touch src/main.rs → build → powershell publish-xxsshg-X.Y.Z.ps1（xxssh 根目录）
- 诊断：XXSSHG_DEBUG_CURSOR=1（光标覆盖层+日志 ~/.xxssh/cursor-debug.log）、XXSSHG_PTY_DUMP=文件（原始字节）
