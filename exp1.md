# xxsshg 开发与发布经验

## eframe/tokio：Runtime 绝不能移进 App 创建闭包

### 问题
GUI 里所有 SSH 后台任务"写入成功但永远没有结果"，连接永远转圈，且无任何报错。

### 根因
eframe 的 AppCreator 闭包（`Box::new(move |cc| ...)`）在创建完 App 后即被丢弃。为图方便把
tokio `Runtime` 本体 move 进了该闭包 → Runtime 随闭包一起 drop → **所有后台任务被静默终止**
（tokio 任务被 drop 不产生任何日志），oneshot sender 随之关闭。

### 修复
Runtime 留在 main 作用域保活（其生命周期覆盖 run_native），只把 `rt.handle().clone()` 传给 App。

### 关键点
- tokio 任务静默死亡时，`oneshot::Receiver::try_recv()` 返回 `Closed`（而不是 Empty）——
  据此在 UI 层报"后台任务崩溃"才能让这类问题显形。
- 通用规则：**长生命周期资源（Runtime/连接池）绝不能进入短生命周期闭包**。

---

## egui 终端渲染：宽字符前的 ASCII 运行段被丢弃

### 问题
中英混排行里，中文前面的英文（含 shell prompt）整段消失；纯英文行、纯中文行都正常。

### 根因
按"样式相同的连续字符"分run渲染时，宽字符分支先 `run_style.take()` 再 flush——
take 把样式置 None，flush 判定"无样式"直接把累积的文本丢弃。

### 修复
宽字符分支**先 flush（带原样式）再单独绘制宽字符**；并修正宽字符后 `run_col` 需 +2
（跟随的 spacer 格被 skip，不修正会导致后续文本左移一格）。

### 关键点
中英混排是终端渲染的必测路径；纯英文/纯中文各自正常不代表混排正常。
回归方法：抓取真实 PTY 字节流回放进网格模型断言（XXSSHG_PTY_DUMP / 回放测试）。

---

## egui 终端：文字与光标步进不一致导致光标"漂移"

### 问题
光标块与文字错位，来回移动光标/放大字号后尤其明显。

### 根因（两层）
1. 格子宽度用 `glyph_width('M')`——是**墨迹宽度**（含边距的 advance 才是排版步进），
   文字按 advance 排、光标按墨迹宽走格子，误差沿行累积。
2. 即使改用排版步进测量，布局引擎与绝对格子坐标仍可能有微小出入。

### 修复
**彻底放弃整行连排，每个字符直接画在自己格子的绝对坐标上**（等宽终端本来就是网格）。
文字与光标同源，机制上不可能错位。性能：只画非空格格 + 仅在有输出时重绘，实测无压力。

### 关键点
行高也不能拍脑袋（1.25×字号），用 `fonts.row_height(font_id)`（字体度量）；
否则行间渗墨、光标块与文字错位随字号放大。

---

## egui 终端：Windows IME 中文输入

### 问题链（按发现顺序）
1. 候选框不出现 → egui 默认不给窗口开 IME，需要
   `ctx.send_viewport_cmd(ViewportCommand::IMEAllowed(true))`（聚焦终端时开、失焦时关），
   并用 `IMERect` 把组词窗钉到光标处。
2. 上屏中文进不去 → IME 提交走 `Event::Ime(ImeEvent::Commit)`，不是 `Event::Text`，两个都要处理。
3. 拼音编辑时删掉已输入文字 → **egui 只在"自家 TextEdit 聚焦"时才过滤被 IME 接管的按键**；
   原生焦点控件必须自己跟踪组词状态（Preedit 非空 = composing），组词期间丢弃所有
   Key/Text 事件。注意同一帧内 Key 事件排在 Preedit 之前，需先扫描 Preedit 再处理按键。

### 正确的组词体验
把 Preedit 文本**内联渲染在光标处**（真实终端的做法）：用户能看到拼音、能退格编辑，
空格上屏发中文、回车上屏发原始字母。字体回退（CJK fallback FontData）+ 组词补偿一起做。

### 关键点
不同输入法事件路径有差异，换输入法后必须重测；测试时先确认跑的是最新编译产物
（标题栏带 build hash 就是为此）。

---

## russh/ConPTY：主机指纹与本地终端

### 指纹信任持久化
交互确认接受指纹后写入 `~/.xxssh/known_hosts`（OpenSSH 格式）。注意 russh 的
`learn_known_hosts_path` **只追加不替换**：接受变更密钥时必须先删除该 host 旧条目
（用 `known_host_keys_path` 拿行号过滤重写），否则之后每次连接都报 KeyChanged。

### ConPTY 本地终端（portable-pty）
- 会话句柄适配成与 SSH 相同的 SessionHandle 形状，终端控件零改动复用。
- **slave 句柄必须保活到会话结束**（Windows 上 drop slave 会弄坏 master 写入，wezterm#4206）。
- **ConPTY 启动握手**：conhost 发 `ESC[6n` 并阻塞等待终端应答后才继续交付输出——
  终端模型（alacritty）会自动应答，测试裸通道必须手动应答，否则子进程卡死在启动。
- ⚠️ **Windows 26200（insider）输入注入回归**：向 ConPTY input 管道写入成功（WriteFile Ok）
  但按键不达子进程；plain/win32-input-mode 编码/UTF-16/成对 down-up/9001l/换 crate 版本
  全部无效；微软自家 WT 正常（microsoft/terminal#19153 同类）。稳定版待验证。
- clink（cmd AutoRun 注入）在 ConPTY 下自动生效，无需额外工作。

---

## 仙缘渡发布（GUI 变体挂同一卡片）

### 模式
GUI 变体作为**同产品的额外平台**发布（如 xxdmg→xxdm）：PRODUCT_ID 用主产品 id，
`PLATFORM = "Windows GUI"`，版本号独立（GUI 自己的 0.1.x，与 CLI 无关）。
不要建独立产品卡片。

### API 要点
- 登录：POST /api/admin/auth（topget）；产品元数据：POST（新建）/PUT（更新，body 带 id+字段即可）
- 图标：POST /api/admin/upload?type=icon；文档：action=addDoc/updateDoc（先 GET 查 doc id）
- 版本：action=addVersion → upload(type=package&versionId=) → action=updateVersion（sha256+isLatest）
- **删除单版本**：DELETE action=version&confirm=DELETE-VERSION
- **删除整个产品**：DELETE action=deleteProduct&id=X&confirm=DELETE-PRODUCT（连版本/文件/docs 全删）
- 安装端点（/install/<id>.ps1）按产品 id 生成，GUI 变体没有（用户从卡片直接下载），与 xxdmg 一致

### 踩坑
- 发布脚本的 Verify/横幅字符串是硬编码的（复制自别的项目），改完脚本要全局搜一遍旧产品名，
  否则"验证输出"会误导排查方向（实际发布是对的，验证打印的是别家卡片）。
- 发布脚本必须 UTF-8 **BOM**（PowerShell 5.x 无 BOM 按 GBK 读中文乱码）。
- 新产品先 DryRun（-DryRun 参数）再正式发布；二进制要有 --version（安装脚本用它判断是否重下）。
- Cargo.toml 版本号必须与发布版本一致 + touch src/main.rs 强制重编（build hash 才是新的）。

---

## 安全：测试凭据与 git 历史

- 测试服务器地址/密码**绝不硬编码**——E2E 测试一律环境变量注入（XXSSHG_TEST_HOST/USER/PASS），
  不设置则跳过。曾把 root 密码写进测试源码并随提交进入历史。
- 清理用 `git filter-repo --replace-text`（filter-branch 的 tree-filter+sed 在 Windows 下
  会静默失败——历史哈希变了但内容没换，必须用 grep 复核）。清理后 reflog expire + gc，
  并对 `git log --all -p` 全文 grep 复核为 0 才算完成。
- 微软系统字体（Consolas/msyh）只能运行时从用户系统加载，**不得打包分发**；
  要内置字体选 OFL/MIT 授权的（Cascadia Code 微软官方 MIT）。

---

## 调试方法论（这次会话沉淀）

1. **字节流仲裁**：UI 争议（光标/渲染/回显）用 XXSSHG_PTY_DUMP 抓真实字节流，
   离线回放进网格模型断言——把"看起来不对"变成可断言的字节问题。
2. **无头自动化**：XXSSHG_AUTOCONNECT=名字 启动即连，配合 RUST_LOG 让"连不上"类问题
   不依赖手点即可复现。
3. **先量测再改**：截图逐像素测量（文字步进 vs 光标步进）比反复猜快得多；
   本轮 6% 步进偏差就是量出来的。
4. **每个修复核对编译产物**：文本替换可能静默失败（锚点不匹配），改完必须
   grep 确认落进源码 + 重新编译 + 让用户重启验证。

---

## 2026-09（v0.3.0→v0.5.0）：SFTP 成熟化 / 重连 / 诊断 / 自我更新

### 状态守卫标志必须"闭环"

**问题**：SFTP 首次列表后双击进目录无反应、传输完成后列表不刷新、删除后不更新——"面板点不动"。

**根因**：`spawn_refresh_remote` 入口的 `busy` 守卫置 true 后，**没有任何路径把它复位**（列表结果到达时不清理）。首屏之后所有刷新请求都被守卫吞掉；新建文件夹还额外发一条"空列表"消息把面板清空。

**关键点**：任何"进行中"标志必须与它的完成/失败路径成对审计——列出所有异步出口，确认每条都会走到复位点。守卫拦截是静默失败，比崩溃更难发现；给拦截加一行日志能在首轮排查就暴露。

---

### 引擎参数语义：调用层没拼文件名，单文件传输全挂

**问题**：单文件上传/下载 100% 失败，文件夹传输却正常。

**根因**：`upload_file/download_file` 接收"完整目标路径"，但调用层把**目标目录**直接传了进去（没 join 文件名）。递归函数内部自己拼名，所以只有目录能跑通。

**关键点**：引擎函数"接收文件还是目录"必须在签名/文档里唯一化；拼名逻辑下沉到一个入口（后来的 `free_local_name/free_remote_name`），让"目录+文件名→路径"只有一种写法。

---

### 同产品卡多构建：版本选择必须先过滤自己的平台

**问题**：`--update` 冒烟测试直接报"no Windows GUI entry at v0.5.2"——商店 GUI 明明有 0.4.0。

**根因**：xxsshg 与 xxssh TUI 挂同一张产品卡，**版本号独立推进**（TUI 已 0.5.2，GUI 0.4.0）。照搬 xxssh 的"全平台最高 isLatest"选法，选出来的是 TUI 的版本号，再按平台找条目自然找不到。

**修复**：先过滤 platform 含 `windows+gui` 的条目再取最高。普通 "Windows" 条目是 TUI，永不匹配。

**关键点**：复用协议时，"产品卡构成"变了选法就得重审；冒烟测试打真实商店一次就抓到，比任何纸面审查都快。

---

### winit 图标：一张图喂 16px 标题栏 + 任务栏

**问题**：`>SSH` 字标图标在标题栏 16px 下糊成一团。

**根因**：winit 把 egui 的单一 `IconData` 同时用作 ICON_SMALL（16px）和 ICON_BIG（任务栏），由系统缩放——字标在任何缩放下都不可读。

**修复**：按尺寸分流——窗口 PNG 用无字简化形（深色方块 + 矢量粗描边 chevron + 白色光标条），exe ICO 逐尺寸生成（≥64 全设计，≤48 简化形）。

**附带教训**：
- 小尺寸**字形墨迹不可靠**：Consolas 的 ">" 墨迹只有字号的一小半，32px 下小得像点；改手绘矢量折线（圆头粗描边）。
- 简化形用 4x 超采样 + LANCZOS 缩小，边缘干净。
- **期望值不能现编**：sha256 测试的期望摘要必须用独立工具现算（PowerShell），凭"印象"写必错。
- Windows 图标缓存会导致"改了没变化"的错觉；标题栏/任务栏随新进程刷新，资源管理器要重启 explorer。

---

### russh 断线真因：`Handler::disconnected` 是唯一通道

**问题**：会话莫名关闭，页签只显示"connection lost"——channel.wait() 返回 None 时不携带任何原因，russh 内部的 KeepaliveTimeout / 服务器 DISCONNECT 原因串全部丢失。

**修复**：实现 `client::Handler::disconnected` 钩子（服务器 DISCONNECT 带原因串；内部错误如 KeepaliveTimeout 以 `DisconnectReason::Error` 到达），把原因存入共享槽；会话任务结束时输出 forensic 行：`reason + cause + uptime + idle`。

**取证解读法**（写进用户文档了）：
- `cause=KeepaliveTimeout` 且 `idle` 约 60~80s → 链路静默死亡（断网/睡眠/NAT 失效），保活探测 4 轮判死；
- `server sent DISCONNECT #N: 消息` → 服务器主动踢，看消息与断开码；
- `idle≈uptime` → 整个会话零交互（心跳一直在往返）。
- 实测 xhw 案例即第一种：5 小时空闲后 KeepaliveTimeout，OS 无睡眠/网络事件 → 指向本机网络路径（未定论）。

**关键点**：库的"完成信号"（None/Err）与"原因"经常分属两层，接库时先把原因钩子接出来，否则排查只能靠猜。

---

### 失败分支的日志覆盖与成功分支同等重要

**问题**：tk8 五次连接全部失败，日志只有 5 行 connecting、0 行原因——握手/认证失败路径没接日志。

**修复**：握手超时/握手错误/认证失败全部落盘 session-debug.log（auth 失败连 AuthErr 枚举都要补 Debug derive 才能打印）。

**关键点**：加日志时按"连接状态机"逐状态核对：connecting/authenticated/transport closed/session end/握手失败/认证失败，缺哪个补哪个。

---

### 手抄算法必须配已知答案测试

update.rs 内联 sha256 抄自 xxssh。第一遍我凭记忆重写了压缩轮（写成了错误的混合式），**没有测试的话 sha256 校验就是摆设**（坏哈希会让所有下载校验失败或形同虚设）。复制后立即补三条已知向量（空串 / "abc" / 576 字节多块），期望值用 PowerShell 独立计算。多块路径必须覆盖（只测空串和 "abc" 不会走到 64 字节以上的分块逻辑）。

---

### 运行中的 exe 锁死 release 构建：用户"以为更新了"

Windows 下运行中的 exe 不可写，`cargo build --release` 链接阶段报 os error 5——本会话踩了四次。更糟的是用户以为磁盘上是新版（实际还是旧的），白测一轮。

**处置**：构建失败立即明确告知"磁盘上仍是旧版"，请用户关闭程序后重编并当场跑 `--version` 核对 build hash；发布脚本跑之前先确认没有运行中的实例。proc.md 速查已加这条。

---

### egui：静止的指针不产生重绘

拖拽选择越界自动滚动第一版"只滚一格就停"：指针停在边缘不动时没有输入事件，egui 不重绘，每帧逻辑自然不跑。**自动滚动这类"按帧推进"的逻辑必须显式 `ctx.request_repaint()`**（在判断到越界的分支里调用）。

---

### UI 反馈闭环：异步操作要"能说清结果"

"进度条到 100%"不等于完成反馈——用户不知道成败、数量、耗时。引擎四个传输函数的返回值从 `u64` 升级为 `TransferStats{files,bytes,skipped}`，UI 才能写出"✓ 上传 photos：12 个文件，48.6 MB，22.4 秒，跳过 2 个"。**引擎返回值的设计决定 UI 能说什么**；加"跳过计数"直接支撑了冲突策略的日志行。成果行统一 ✓/✗ 前缀 + stick_to_bottom 自动滚动，用户不再需要去文件列表核对。

---

## 2026-09（续）：更新器实战与 egui 陷阱补遗

### egui：`Window::open(false)` 是"隐藏整个窗口"，不是"隐藏关闭按钮"

更新对话框想表达"下载中禁止关闭"，写成 `open=false`——结果整个对话框（含进度条）在下载期间消失，观感是"弹窗转瞬即逝"。要禁关闭仍显示窗口：**不挂 `.open()` 即可**（无 X 按钮）。egui 的 open 是三态语义：`Some(true)` 显示带 X / `Some(false)` 隐藏 / 不设置显示无 X。

### 覆盖引擎内部顺序会毁掉调用方的"自然顺序"

`list_dir` 内部图方便按名称排序，结果三态排序的第三态（取消排序、回自然顺序）重新拉列表后**看起来和名称排序一模一样**。修复：引擎不做任何排序，顺序决定权交还调用方。教训：引擎/IO 层不要"顺手"做呈现层的事——你顺手排的序，就是调用方永远拿不到的原始数据。

### 自更新流程的现场取证法

文件系统就是更新流程的日志：
- `<exe>.new` 存在且 0 字节 → 死在下载阶段（本轮真因：连接慢/卡 + 单次尝试）
- `<exe>.new` 存在且大小正常 → 死在校验或替换阶段
- `<exe>.old` 存在 → 替换已成功，只是清理没跑成（运行中进程锁住旧镜像，延迟删除必然失败）
- 都不存在且版本没变 → 死在检查阶段（网络/API）

修复相应为：失败清理 .new、重试 3 次、启动时清扫 .old。

### 延迟删除 vs 运行中进程锁

`ping -n 3 & del` 式的延迟清理假设"2 秒后程序已退出"——GUI 里用户可能一直开着程序，删除必然失败且无日志。凡"等程序退出后清理"的模式，可靠做法是**下次启动时清理**，而不是赌用户恰好退了。

### 观感问题也是问题

"窗口自动关闭、重开显示已是最新"——流程 100% 正确，用户依然迷惑。反馈闭环不仅指结果数据，还指**过程可见性**：看不见的下载等于没在下载。修复后（进度条全程可见）这类迷惑自然消失。
