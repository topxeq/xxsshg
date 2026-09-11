# xxsshg 开发进度记录

> 本文件记录开发会话进度，便于跨会话续接。最新在最前。
> 项目：xxsshg——xxssh 的 GUI 版（独立代码库 repo-gui，与 xxssh 共用 ~/.xxssh 配置）。
> 设计方案见上级目录 prd2-xxsshg.md。

---

## 2026-09-05 发布会话（v0.7.2）：彩色空格背景修复

### 修复（0.7.1 → 0.7.2）
- **彩色区域里的空格留洞**：绘制循环的"空白格快速跳过"发生在背景填充之前，凡只设背景色（无反显）的空格单元格背景从未被画出——apt 进度条 `Progress: [ 99%]`、htop 表头等彩色运行段中每个空格一个洞
- 修复：背景填充提前到任何跳过之前；空白快速跳过只作用于字形绘制且额外要求背景为默认色（性能不变）
- 用户实测确认修复生效（apt 进度条连续着色）

## 2026-09-05 发布会话（v0.7.1）：终端双击/三击选择

### 功能（0.7.0 → 0.7.1）
- **终端鼠标多击选择**：双击选中空白定界的单词（宽字符含占位格一体计词，中英文均正确），三击选中整行（去行尾空白）；复制跟随「选中即复制」设置
- 修复：旧实现注释写"选词"实际只选中单个字符

## 2026-09-05 发布会话（v0.7.0）：服务器栏排序 + 断线页关闭

### 功能（0.6.3 → 0.7.0）
- **服务器栏排序/手动调整**：Servers 标题旁排序按钮（名称升序 ▲ → 降序 ▼ → 恢复手动顺序，三态）；右键菜单「上移/下移」手动调整顺序；两者都立即写回 servers.json（xxssh TUI 共享同一份配置）；排序是视图不落盘，手动调整才落盘
- **断线页「关闭」按钮**：重新连接旁边加关闭页签（本地终端退出页也有关闭、无重连）
- 按钮文字全部用文本标签（⇅ 字形在 UI 字体缺字渲染成空心方块——字符选择要考虑 UI 字体覆盖）
- 测试：38 全过

### 发布
- v0.7.0 发布到仙缘渡 xxssh 卡 Windows GUI（publish-xxsshg-0.7.0.ps1）；清理 0.6.3

## 2026-09-05 会话：xxssh TUI 自更新修复 + 四平台重发布（v0.5.3）

### 触发与修复（xxssh repo，dce7b84）
- 用户 Ubuntu 上 `xxssh --update` 报 "No matching version for the current platform at v0.6.3"：xxssh 的自更新在全平台取最高 isLatest，取到 GUI 的 0.6.3（同卡版本线独立）→ 与 xxsshg 完全同源镜像的 bug
- 修复：`current_platform_versions` 先按本平台过滤（Windows 上排除 "Windows GUI" 平台条目）再取最高；测试覆盖该失败场景（含宿主自适应）
- xxssh v0.5.3 发布：Windows/Linux x64（本机）、Linux ARM64（cargo zigbuild musl 交叉）、macOS universal（GitHub Actions 构建）

### 事故与恢复：TUI 发布脚本跨平台误删
- publish-0.5.3.ps1（沿袭旧模板）的"清理旧版本"段删光了其他平台：Linux ARM64/macOS 0.5.2 与 **Windows GUI 0.6.3** 被误删（商店一度只剩 Windows 0.5.3）
- 恢复：Linux x64 0.5.3 立即重发；ARM64 用 zigbuild 重编发布；Windows GUI 0.6.3 重跑 xxsshg 发布脚本；macOS 经 GitHub Actions 构建后下载发布——五平台全部恢复 isLatest
- 新工具 cleanup-tui-platform.py（平台限定清理，参数=保留版本+平台列表）；0.5.3 各发布脚本已全部换成平台限定版

### CI 认知修正
- **多平台二进制一直在 GitHub Actions 构建**：repo/.github/workflows/release.yml 推 `v*` tag 触发（Windows / Linux x64 musl / Linux ARM64 musl / macOS universal lipo），产物挂 GitHub Release；build-arm64.yml（workflow_dispatch）用于 ARM64 补发
- magicdo.top 发布仍是本地脚本：从 GitHub Release 下载后转发布

### 遗留
- 已发布的 0.5.2 及更早 TUI 自更新二进制逻辑仍是坏的（跳不过 0.5.2），存量用户需走 install 脚本或手动下载一次到 0.5.3+
- ARM64/macOS 若无新版要发，暂无影响；下次 TUI 发版四平台应一起出

## 2026-09-05 发布会话（v0.6.3）：终端自动聚焦 + 字体清晰度设置

### 功能（0.6.2 → 0.6.3）
- **终端自动聚焦**：新建页签（SSH/本地/重连）与切换页签自动获得键盘焦点，无需点击即可输入；焦点状态变化写 session-debug.log（用于区分"没焦点"与"会话死亡"）
- **「字体清晰渲染」设置项**：设置对话框新增勾选（默认开），关闭 = egui 平滑渲染；保存立即重装字体生效
- **Mono hinting 目标（发虚真凶）**：skrifa 探针实测 Consolas 在 Smooth 模式下 `preserve_linear_metrics` 开/关输出逐位相同（ClearType 字体设计上不做水平拟合），Mono 目标才横向网格对齐 → 锐利模式拉丁字体用 Mono 目标，CJK 回退保持 Smooth（Mono 拟合会断中文笔画）；顺带修复误删的 cjk_fallback 注册

### 发布
- v0.6.3 发布到仙缘渡 xxssh 卡 "Windows GUI"（publish-xxsshg-0.6.3.ps1）；清理 0.6.2

## 2026-09-05 发布会话（v0.6.2）：文字清晰度

- **根因**：egui 0.36 默认 hinting 配置 `preserve_linear_metrics: true` 关闭水平网格对齐——上游文档明言竖直笔画在低 DPI 屏幕会发软
- **修复**：终端等宽字体 + CJK 回退改用 `SmoothHinting { light: false, preserve_linear_metrics: false }`（全量网格对齐）；egui 用 shaper advance 排版，只影响锐度不影响布局，格子度量不变
- **字号吸附物理像素**：125% DPI 下 14pt=17.5px 光栅化，小数尺寸本身发虚——吸附为整数物理像素（→18px）
- 诊断：渲染指标（ppp/字号/格子）变化时写 session-debug.log，便于继续调优
- **启动清扫 `<exe>.old`**：延迟清理在程序运行时被进程锁挡住，改每次启动 best-effort 清理（v0.6.1 会话的遗留尾巴）
- 测试：37 全过；已知取舍——hinting 会轻微改变字形形状（更硬朗），不满意可回退该项单独保留字号吸附

## 2026-09-04 发布会话（v0.6.1）：更新器加固

- 实测用户自更新失败：下载 URL 有效（完整下载 127s/17.8MB，sha256 吻合），但连接会卡顿/重置——单次尝试 + 300s 总超时导致失败，且失败后残留 0 字节 `.new` 文件（排得的现场证据）
- 加固：下载失败自动清理 `.new` + 重试 3 次（间隔 1.5s，进度条从头重走）+ 超时放宽到 600s
- **启动时清扫 `<exe>.old`**（cf927ad）：延迟 2s 的清理在程序仍运行时触发必然失败（旧镜像被进程锁住），GUI 自更新后 `.old` 永远残留 → 改为每次启动 best-effort 清理
- **用户实测闭环**：0.5.0 → 自更新成功到 0.6.0（二进制验证），但 v0.5.0 下载期间对话框不可见 + 「立即重启」关窗，观感即"窗口自动关闭、重开显示已是最新"的迷惑——0.6.1 起进度全程可见，此迷惑消除
- 经验：curl 全量下载 + Get-FileHash 对照是验证商店文件完整性的最快手段；`.new`/`.old` 文件是自更新流程的现场证据（0 字节 .new = 下载阶段失败；.old 存在 = 替换已完成）

## 2026-09-04 发布会话（v0.6.0）：系统拖放上传 + 双栏排序

### 功能（0.5.0 → 0.6.0）
- **系统级拖放上传**：从资源管理器拖文件/文件夹到窗口，SFTP 页签激活时上传到远程当前目录（多选/递归均支持，冲突策略照常）；终端页签忽略防误操作；远程面板悬停绿色高亮 + "释放以上传"提示；操作日志记"接收拖放 N 个项目"
- **双栏三态排序**：列表上方排序条（名称/大小/修改时间/创建时间），升序 → 降序 → 自然顺序循环（▲/▼ 指示）；文件夹始终置顶；远程"创建时间"回落为修改时间（SFTP v3 无该属性）
- 复查修复：更新对话框在下载期间整个隐藏（egui open=false 语义）→ 下载中不挂 .open 且无关闭按钮；远程"自然顺序"被 list_dir 内部排序破坏 → 引擎不再预排序；纯跳过传输日志行改为"已跳过 x（同名冲突）"；清空按钮文案

## 2026-09-03 发布会话（v0.5.0）：自我更新 + 拖拽选择自动滚动

### 功能（0.4.0 → 0.5.0）
- **自我更新**（对齐 xxssh `--update` 协议，同卡片 `api/products?id=xxssh`）：
  - ☰ 菜单「检查更新」对话框：检查 → 询问 → 下载进度条 → ✓完成；「立即重启」spawn 新 exe 后退出；下载中禁关闭
  - `--update` CLI（console 流程；GUI 子系统下无输出，菜单为主路径）
  - 替换流程同 xxssh：下载 `<exe>.new` → sha256 校验（内联实现抄自 xxssh，已知答案测试守护）→ rename 替换 → 延迟删 `.old`
  - ⚠️ 关键差异：GUI 与 TUI 同卡不同步发版，「全平台最高 isLatest」的选法会选中 TUI 的版本号 → 必须**先过滤 platform 含 windows+gui 的条目**再取最高（--update 冒烟测试当场抓到）
- **拖拽选择自动滚动**：选择时鼠标越过上下边缘每帧滚 1~8 行（按超出距离），端点骑新边缘，可跨屏选择；alt-screen 跳过

### 发布
- v0.5.0 发布到仙缘渡 xxssh 卡 "Windows GUI"（publish-xxsshg-0.5.0.ps1，16.96 MB）；清理 0.4.0 条目
- 自我更新闭环验证：发布后跑 `--update`，当前版本 ≥ 店内 GUI 最新 → 正确报 Already up to date

### 已知问题 / 待办（v0.5.0 后）
- **tk8（47.91.31.109:22）五连未认证**：独立问题待查——0.5.0 起握手/认证失败会写 session-debug.log，用户复现一次即可取因
- 断线根因未定论：xhw 等 KeepaliveTimeout 时机器未睡眠、无网络切换事件，指向网卡节能/路由 NAT（应用侧已闭环：检测+一键重连）
- `--update` 在 release（GUI 子系统）下无控制台输出，菜单对话框为主路径

## 2026-09-03 发布会话（v0.4.0）：重连 + 操作日志 + 断线诊断 + 新图标

### 功能（0.3.0 → 0.4.0）
- **传输同名冲突策略**：覆盖/跳过/重命名 + 全部粘性决策，逐项排队询问；Decision 枚举 + free_local/remote_name 找空名（与新建文件共用）；临时预览文件固定覆盖
- **断线重连**：断线页「重新连接」按钮 + 页签右键菜单「重连」（SSH 终端含已断开/连接失败页签/SFTP 页签；本地终端不显示）；SftpTab/Failed/SftpConnecting 携带 Server 原地重开
- **SFTP 操作日志框**：底部面板（进行中传输进度条 + ✓/✗ 结果行，stick_to_bottom 自动滚动）；引擎返回 TransferStats{files,bytes,skipped}，完成行含文件数/总量/耗时/同名跳过数；本地删除/重命名/新建与错误均入日志；完成传输行图标变 ✓
- **断线诊断**：常开 ~/.xxssh/session-debug.log（连接里程碑/传输层关闭/会话终局 uptime+idle）；russh Handler::disconnected 钩子拿回丢失的真因（服务器 DISCONNECT 原因串、KeepaliveTimeout）；握手超时/握手错误/认证失败全落盘。实测结论：xhw 等两条会话 idle≈uptime 后 KeepaliveTimeout——服务器未踢、机器未睡眠，指向本机网络路径（未定论）；tk8 五连未认证（独立问题待查）
- **新图标**：终端窗口 + `>SSH` 字标（assets/gen_icon.py，Pillow 生成）；winit 单图喂 16px 标题栏+任务栏 → PNG 用无字简化形（矢量 chevron+光标），ICO 按尺寸分流（≥64 全设计，≤48 简化形）
- 服务器右键菜单：「SFTP 文件管理」移到最下（分隔线隔开）

## 2026-09-01 开发+发布会话（v0.3.0）：SFTP 文件管理器成熟化

### 本会话功能（0.2.0 → 0.3.0）
- **SFTP 右键菜单**：左右栏条目各自弹出（本地上传/打开/快速查看/属性/新建文件夹/重命名/删除；远程下载/打开/快速查看/属性/重命名/删除）；右键同时选中条目
- **快速查看**：文本预览窗口（1 MiB 截断提示、二进制检测、自动换行开关）；远程文件先取到 %TEMP%\xxsshg-view（预览上限 16 MiB，打开不限）
- **属性窗口**：本地（名称/位置/类型/大小/修改时间）+远程（加权限八进制/所有者/属组，SFTP stat）；文件夹大小后台计算，"正在计算"提示持续到算完（PropsAppend done 标志）
- **新建文件/新建文件夹**：两栏工具栏+右键菜单；重名自动 `(2)` 后缀（资源管理器风格）
- **传输同名冲突策略**：对齐 xxssh TUI——覆盖/跳过/重命名 + 全部粘性决策，逐项询问排队；Decision 枚举；free_local/remote_name 找空名；临时预览文件固定覆盖
- **清空缓冲区**：Ctrl+Shift+K（焦点终端）+ ☰菜单 + 页签右键；显示层注入 CSI H/2J/3J，不进 PTY，远端程序无感
- 弹窗体验：重命名/新建输入框回车确认、Esc 取消、打开即聚焦（修了聚焦标志不复位）；快速查看/属性窗口居中锚定

### 重要修复（详见 exp1.md）
- **busy 标志永不复位** → 首屏后所有远程刷新（进目录/传输后/删除后）全部被吞——SFTP"点不动"的真凶；op_mkdir_remote 还发空列表清空面板
- **单文件上传/下载必失败**：把目标目录当文件路径传给引擎（没拼文件名）
- canonicalize(".") 覆盖请求路径 → "上级"按钮列原地（二遍复查抓到的自己引入的回归）
- 本地"打开"走 cmd start → 远程文件名元字符注入本地 shell，改 explorer.exe
- confirm_ui 每帧渲染两遍（app.rs 尾部残留调用）
- 本地删除大树卡 UI 线程 → spawn_blocking；选中项按名重映射；名称校验（空/路径分隔符/Windows 非法字符）

### 发布
- v0.3.0 发布到仙缘渡 xxssh 卡片 "Windows GUI" 平台（publish-xxsshg-0.3.0.ps1）
- 注意：cleanup-latest-xxsshg.py 的 PRODUCT_ID='xxsshg' 是独立产品卡时代的残留，会删光 xxssh 卡全平台旧版本——清理必须用带 platform="Windows GUI" 过滤的适配版

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
- ⚠️ 发布前必须关闭运行中的 xxsshg：exe 被占用时链接报 os error 5，**磁盘上仍是旧版**（已两次踩到）
- 诊断：XXSSHG_DEBUG_CURSOR=1（光标覆盖层+日志 ~/.xxssh/cursor-debug.log）、XXSSHG_PTY_DUMP=文件（原始字节）
- 会话诊断日志：~/.xxssh/session-debug.log（常开、UTC 时间戳；断线真因/uptime/idle/握手认证失败原因）
- 自我更新：`xxsshg.exe --update`（console 流程）或 ☰ 菜单「检查更新」；`--update` 直打真实商店可当冒烟测试
- 图标再生成：python assets/gen_icon.py（输出 xxssh-icon.png/.ico + preview*.png；调色/改字标改脚本常量）
