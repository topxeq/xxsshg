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
