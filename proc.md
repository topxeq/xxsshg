# xxsshg 开发进度记录

> 本文件记录开发会话进度，便于跨会话续接。最新在最前。
> 项目：xxsshg——xxssh 的 GUI 版（独立代码库 repo-gui，与 xxssh 共用 ~/.xxssh 配置）。
> 设计方案见上级目录 prd2-xxsshg.md。

---

## 2026-10-17 发布：CLI v0.5.6（SOCKS5 修复）+ GUI v0.10.0（隧道管理界面）

- CLI：6591db6（SOCKS5 垃圾应答修复 + 0.5.6 + conf.json 描述）→ tag v0.5.6 → release.yml 全绿（run 38999187542）→ 4 个 publish-0.5.6*.ps1 逐平台发布，vid：Windows ver-17922084422860c2 / Linux x64 ver-17922084852b95ed / ARM64 ver-1792208494e02164 / macOS ver-17922085025d92d8；各平台 0.5.5 已删
- GUI：24224e2（隧道一期/二期 + 0.10.0）→ touch 重编 → `--version` 核对 `v0.10.0 (24224e2)` → publish-xxsshg-0.10.0.ps1 → vid=ver-1792208131c5cb29，18.08MB，sha256 9b05a0a4…bf6c；cleanup 删 0.9.0
- 公网验证：latest 5 平台 = Windows GUI v0.10.0 + CLI 四平台 v0.5.6；install 端点 .ps1/.sh 均 0.5.6；实下 xxsshg.exe sha=9b05a0a4 与记录一致、运行输出 `v0.10.0 (24224e2)`；实下 xxssh.exe sha=469caf71 一致、输出 0.5.6；Linux x64 实下 sha=dd91e0e0
- **踩坑（新）**：publish 脚本中途重跑产生了 Windows v0.5.6 **重复条目**（两个 vid 都 isLatest；脚本的已有版本复用检查没接住，cleanup 只删"非本版本"拦不住同版本重复）——用 cleanup 的 DELETE 接口按 vid 手工删掉多余条目后恢复正常。教训：发布脚本一次跑完，确需重跑时先查 /api/products 是否已有该版本
- 已推送：github.com/topxeq/xxssh（e551a21..6591db6 + tag v0.5.6）、github.com/topxeq/xxsshg（7d99836..24224e2）

---
---

## 2026-10-17 自查修复（GUI+CLI）：隧道一期/二期代码全面复查，修 6 处

逐文件复查（借用/生命周期/边界条件/i18n 完整性脚本交叉比对），发现并修复：

- **A（严重，泄漏）终端会话的隧道监听不随会话消亡**：PTY 泵任务结束（断线/关标签）从不清理 controller——server.tunnels 的监听端口会永久占用。修复：controller 传入泵任务，结束时 `abort_all()`（open_pty_after_auth 签名改为收 shared+controller，connect_and_open 负责建立）
- **B（bug）-R 端口 0（服务器分配）时远端表清理错键**：remove/abort_all 用 spec.port 清 `RemoteTable`，而表键是实际 bound 端口。修复：ForwardRuntime 增加 `remote_bound: Option<u32>`
- **C（bug，CLI 同修）SOCKS5 垃圾输入误发应答**：握手 Err(rep) 对 rep==0（协议垃圾/方法已回 0xFF）也写应答字节——方法拒绝路径会连发两个应答，垃圾连接会收到"成功"。修复：`Err(rep) if rep != 0`（repo 与 repo-gui 两份引擎同改）
- **D（UX）快速隧道临时目标 host 为空不拦截**：补 err_host_required 校验
- **E（egui）面板 grid id 冲突**：隧道标签页与浮窗同帧渲染时 `tunnel_rows`/`tunnel_add_grid` id 相同——`tunnel_panel_ui` 增加 id_prefix 参数
- **F（bug）`col_port` 键缺失**：添加转发表单端口标签渲染为 "(?)"——GUI i18n 表补键（CLI 有、GUI 没有）

复查确认无问题的点：conns/流量统计方向语义（↑=客户端→目标）；TunnelConnecting 中途关标签（close_tx drop → 监视器按 by_user 收尾）；快隧去重按解析后规则而非字符串；借用电（渲染器收集意图、borrow 结束统一执行）；`tr()` 引用键与 i18n 表脚本交叉比对 0 缺失。

复验：GUI 63 / CLI 193 测试全过，双侧 release 构建通过，GUI 启动冒烟（本次按 PID 清理）。

---
---

## 2026-10-17 功能（GUI，未发布）：隧道一期+二期——快速隧道、隧道管理面板、动态增删与流量统计

### 底座重构（tunnel.rs）
- `ForwardSet`（一次性任务集合）升级为 **`TunnelController`**：`start/add_forward/remove_forward/list/abort_all`，一条 SSH 会话的转发可随时增删（-R 走 `cancel_tcpip_forward`，用注册时的原始 bind 串）
- **`FwdStats`**：每条转发 conns/up/down（AtomicU64），`copy_bidirectional` 返回值不再丢弃；`conns` 在通道建立时计数
- `RemoteTable` 值升级为 (host, port, stats)；`spec_from_parts`（表单直构）与 `ForwardSpec::to_entry()`（规范存储串，测试锁定与 parse 往返一致）
- `list()` 用 std Mutex、无 await，UI 每帧调用无成本

### session.rs
- `SessionHandle.tunnel: Option<SharedController>`（本地 shell 为 None）——SSH 终端会话即使 0 条配置隧道也带 controller，UI 可随时加
- `spawn_tunnels(rt, server, opts, forwards)`：转发放到显式参数（快速隧道的临时转发不落盘也能起）；`TunnelHandle { controller, event_rx, close_tx }`（lines 字段移除，面板直接 list()）

### 一期 UI（app.rs）
- **快速隧道对话框**：侧栏新增 ⛓ 按钮（布局改为 3 个方形按钮）；服务器右键「快速隧道...」（预填该服务器现有 tunnels）；服务器下拉含「临时 user@host（不保存）」项；**仅本次连接** / **保存到服务器**（按解析后的规则去重合并，不按字符串形态）双动作；逐行校验带行号报错
- **终端 tab 微标**：标题 `name [T n]`（n=list().len()，>0 才显示）
- **终端 tab 右键「隧道...」浮窗**：管理当前会话转发（与隧道标签页共用 panel）

### 二期 UI
- **Tunnels 标签页 → 管理面板**（`tunnel_panel_ui`，与浮窗共用）：每行 ● + 描述（实际端口）+ `连接 n ↑up ↓down` + 💾保存到服务器 + ✕动态摘除；内联「+ 添加转发」表单（D/L/R 切换、目标列语义标签随类型变化、行内校验）
- 异步操作反馈：`ctrl_fb` 通道 → toast（已添加/已移除/失败原因）；`run_tunnel_actions` 在 borrow 结束后统一执行（egui immediate-mode 借用约束的成熟模式：渲染器收集意图、调用方执行）

### 验证
- 63 测试全过（新增 spec_from_parts 校验、to_entry 往返 2 个）；release 构建通过；启动冒烟 5s 无崩溃（注：冒烟按映像名 taskkill 误杀了一个可能在用的 xxsshg 实例——后续按 PID 杀）
- 未发版未提交；待用户验证 UI 后走发布流程

---
---

## 2026-10-17 发布：CLI v0.5.5 四平台 + GUI v0.9.0（端口转发/隧道）

- CLI：e551a21（功能+版本号 0.5.5+conf.json 描述，先提交）→ tag v0.5.5 推送 → release.yml 全绿（run 38993740163，windows/linux/linux-arm64/macos + release 资产）→ 产物下载放置 → 4 个 publish-0.5.5*.ps1 逐平台发布 + 各自 cleanup（0.5.4 条目已删）
- GUI：f9a0110（功能+文档+0.9.0，先提交）→ touch 重编 → `--version` 核对 `v0.9.0 (f9a0110)` → publish-xxsshg-0.9.0.ps1（0.8.9 版复制改版号，BOM 保持）→ vid=ver-17922030702f7527，17.85MB，sha256 3c8dd629…c766；cleanup 删 0.8.9
- 公网验证：/api/products latest = Windows v0.5.5 / Linux x64 v0.5.5 / Linux ARM64 v0.5.5 / macOS v0.5.5 / Windows GUI v0.9.0；install 端点 .ps1/.sh 均 Version: 0.5.5（内嵌 sha 校验一致）；实下 xxsshg.exe sha 与记录一致、运行输出 `xxsshg v0.9.0 (f9a0110)`；实下 xxssh.exe sha=eac33e5a… 与 API/安装脚本一致、输出 0.5.5；抽验 Linux x64 实下 sha=7c03fa28… 与 API 一致
- 已推送：github.com/topxeq/xxssh（8459a36..e551a21 + tag v0.5.5）、github.com/topxeq/xxsshg（3fa0e04..f9a0110）
- 备忘：CLI 四平台产物自此全部走 tag→Actions（release.yml），Windows 也用 CI 产物（同源一致），测试服务器 musl 手工构建路径不再需要

---
---

## 2026-10-17 功能（CLI+GUI）：端口转发/隧道，免终端（-D SOCKS5 / -L / -R / -N + 服务器 tunnels 配置）

### CLI（repo，0.5.4 基础上，未 bump 版本）
- 新增 `src/tunnel.rs` 转发引擎：`ForwardSpec`（Dynamic/Local/Remote）+ 解析（`-D [bind:]port`、`-L/-R [bind:]port:host:rport`，bind 缺省 127.0.0.1，R 缺省 ""）+ 监听任务（`tokio::io::copy_bidirectional` 泵）+ 自实现 RFC1928 SOCKS5 服务端（无认证、仅 CONNECT，远端解析域名=socks5h 语义）
- russh `Handle` 不可 Clone：会话包成 `Arc<tokio::sync::Mutex<Handle>>`（`SharedSession`）供 PTY 循环与全部转发任务共享；`-R` 经 `Handler::server_channel_open_forwarded_tcpip` 回连本地目标（按端口查 `RemoteTable`，慢目标 spawn 泵不阻塞会话循环）
- `ssh.rs`：`run_session` 增参 `(cli_forwards, no_shell)`；每次连接自动带上 profile 的 `tunnels` 列表；`-N` 免终端模式（无 PTY/不进 raw mode，等 Ctrl+C 或连接死亡，建立失败退出码 1）；PTY 与转发互不影响，任一转发建立失败则整个连接失败（不会起一半）
- `main.rs`：`-D/-L/--dynamic/-N/--no-shell` 可重复、可叠加在 profile tunnels 之上；`-N` 不再 require TTY（脚本可用）；help 新增 `--help tunnel` 主题（TOPICS/AGENTS 任务表同步）
- `config.rs`：`Server.tunnels: Vec<String>`（`#[serde(default)]`，条目 `D:1080` / `L:8080:db:5432` / `R:0.0.0.0:80:localhost:3000`）；旧版本 xxssh 读新文件忽略未知字段、重存时丢弃（降级安全）
- TUI 服务器编辑表单第 10 个字段"隧道"（逗号分隔，保存时校验）；i18n 三语补齐（fw_*、field_tunnels、err_tunnel 等）
- Cargo.toml tokio 补 `net`、`signal` feature；193 测试全过

### GUI（repo-gui，0.8.9 基础上，未 bump 版本）
- `tunnel.rs`（引擎同 CLI，错误消息中性英文、类型 pub）；`xconfig.rs` Server 加 `tunnels`（schema 契约测试字段集更新为含 tunnels 的 0.5.4 版）
- `session.rs`：`spawn_connect` 增 `forwards` 参数（带终端连接同时拉起隧道，成功后发 `SessionEvent::ForwardsUp(lines)` → toast）；新增 `spawn_tunnels`/`TunnelHandle`（纯隧道会话：建立→保持→close_tx 断开/掉线通知）
- `app.rs`：新 Tab 变体 `TunnelConnecting`/`Tunnels`（绿点列表显示各监听行，复用连接问答对话框/重连/关闭标签页生命周期）；服务器右键菜单加"仅连接隧道（不开终端）"（无 tunnels 配置时禁用/提示）；服务器行有 `[T xN]` 标记；表单加多行隧道输入（逐行校验）；带终端连接同样自动拉起该服务器的 tunnels
- i18n 三语补齐（menu_tunnels/tnl_*/f_tunnels/err_tunnel）；61 测试全过，构建通过

### 关键 bug（真机发现并修复）
- `-R` 指定端口时 OpenSSH 的 tcpip-forward 成功应答**不带端口号**（仅请求 0 时才回显），russh `tcpip_forward` 因此返回 0 → 映射表错注册到 0 号端口，服务器回连 11999 时查表失败被拒（现象：/dev/tcp 连上但 EOF）。修复：`bound = replied==0 ? 请求端口 : replied`（详见 exp1.md）

### 验证（真机 bt.topget.org）
- `xxssh bt -N -D 127.0.0.1:11080 -L 127.0.0.1:11022:127.0.0.1:22 -R 127.0.0.1:11999:127.0.0.1:22`：三条隧道同会话拉起；SOCKS5 握手+CONNECT 取回远端 sshd banner ✓；-L 读回 banner ✓；-R 由服务器 /dev/tcp 回连穿回本机 22 端口取回 banner ✓；进程存活保持、taskkill 后清理正常

### 未做/后续
- 未发版（按惯例下次发布时 bump 仓库版本+commit+publish 脚本）；-L/-R 目标字段暂不支持 IPv6 字面量（bind 支持 [::1]）；SOCKS5 不支持 BIND/UDP ASSOCIATE（回 0x07）
- 未提交 git（两仓库均未 commit）

---

## 2026-10-22 v0.10.7：上传目录选择框支持新建文件夹

- 需求（用户）：选择框里要有新建文件夹功能
- 交互：底部"新建文件夹"按钮 → 内嵌命名行（自动聚焦、回车确认）→ 在当前浏览目录创建（远端 SFTP mkdir_all / 本地 create_dir_all）→ 自动进入新目录 → "Upload here" 直接落位
- 名称校验：空名/含 / 或 \ 拒绝；创建失败原地显示错误可重试；成功后清提示态
- DirPick 新增 new_dir_prompt/new_dir_name/mk_rx 通道；poll_mk 驱动导航；i18n 增 up_mk_create
- 64 测试全过；vid=ver-1792635453055651，公网 latest=Windows GUI v0.10.7（单条，dl sha 03d979d6 与实下一致）；已推送（d724b81）

---

## 2026-10-22 v0.10.6：每个 profile 记住上次上传目录

- 需求（用户定调）：不做 shell 集成注入；每个 profile 记住上次上传目录，选择框下次从那里开始，首次默认用户目录
- 实现：GuiConfig 新增 `last_upload_dir: BTreeMap<"host:port", dir>`（gui.json，GUI 专用不污染共享的 servers.json）；选择框锚点优先级 = 记忆目录 → home → /；确认上传与 cwd 直传两条路径都会写入并保存
- 本地 cmd/PS 页签不参与（无 profile 概念，锚 USERPROFILE）
- vid=ver-1792634125b568e2，公网 latest=Windows GUI v0.10.6（sha 1c3b849c…，实下校验一致）；已推送
- **重踩 0.5.6 的同版本重复坑**：重跑 publish 时脚本的已有版本复用检查没接住，产生了两个 0.10.6 条目——按 proc.md 记录的处置用 admin DELETE 按 vid 删掉旧条目（错哈希构建）恢复单条
- **构建时序又错一次**：先 build 后 commit，嵌入哈希指向无功能代码的旧树——已重编重传纠正。两次低级失误同因：发版步骤没按 memory 流程逐步走，后续发版严格按 checklist 执行

---

## 2026-10-22 v0.10.5：0.10.4 的 home 等待分支吞掉了第一次拖拽

- 用户实测 0.10.4：第一次拖拽无反应，第二次才弹框（锚 /root 正确）
- upload-debug.log 定位：0.10.4 把 `mem::take(pending)` 提到了 match 之前，而新增的"等主目录"分支里 paths 被直接丢弃——新会话的第一次拖拽撞进 canonicalize 窗口（~30ms）即被无声吞掉；第二次拖拽时 home 已解析完，走正常路径
- 修复：pending 只在确定动作（直传/弹框）时才 take；等待分支只留一个 uplog 标记，队列原样保留下一帧重试
- 64 测试全过；公网 latest=Windows GUI v0.10.5；已推送（848d0ae）

---

## 2026-10-22 v0.10.4：选择框锚定远端主目录而非 /

- 用户实测 0.10.3 正常（不再卡死），但选择框锚在 /：canonicalize(".") 的往返（~30ms）通常晚于选择框打开，anchor=None 回退到 /
- 修复：TermUp 记录 home_done（主目录解析完成标志）；cwd 不可信时**等主目录解析完成再弹框**（pending 多持有一帧），锚点取真实主目录（root 登录即 /root）
- 64 测试全过；公网 latest=Windows GUI v0.10.4；已推送（20ea02c）

---

## 2026-10-22 v0.10.3：抓到真凶——take_failure() 每帧无条件重置状态机（10054 的真正根因）

- upload-debug.log 显示：SFTP 通道每次都在 0.5s 内成功打开，但 connect_via_session 以 ~80ms 周期无限重发——状态机被反复清回 Idle
- **真凶**：`TermUp::take_failure()` 每帧都被调用，实现是 `mem::replace(&mut self.state, UpState::Idle)` **无条件**覆盖——Ready/Connecting 全被清掉，拖拽 kick 看到 Idle 就再开一条通道（0.8.9 时代则是再开一条新 SSH 连接）
- **改写认知**：最初的 os error 10054 根本不是"服务器限制第二条连接"，是我们自己的连接洪流（每几帧一条新连接）逼服务器 RST。via-session 重构仍是净收益（免二次认证等），但当时把根因归给服务器是错的
- 修复：take_failure 加守卫——仅当状态真是 Failed 才重置（此时清 pending、返回错误文案），其余状态零操作
- v0.10.3：vid=ver-1792632220156849，公网 latest=Windows GUI v0.10.3，已推送（191d81d）
- **教训**：每帧轮询的 UI 代码里，任何"读改写"状态的操作必须带前置条件守卫；e2e 单次调用测不出每帧循环才暴露的 bug

---

## 2026-10-22 v0.10.2：拖拽卡 Connecting 的可观测性与看门狗（用户实测中）

- 用户实测 0.10.1：拖文件进 SSH 终端卡在 "opening upload channel on..." 徽标不动
- 排查：via-session 通道对同一台 xhw 服务器的 e2e 0.54s 通过（生产 spawn_connect 全链路），静态审查无异常；GUI 后台自动化复现失败（会话无法创建窗口，且两次拉起的实例已清理——注意 taskkill //PID 需要的是 Windows PID 而非 MSYS job pid）
- 处置：① diag.rs 新增 `uplog` —— 上传链路专用无条件日志 `~/.xxssh/upload-debug.log`（一次拖拽约 10 行：drop/enqueue/connect_via_session/pump 各阶段/on_connect_result/trusted 判定/picker/确认/watchdog/shutdown）；② Connecting 状态加 30 秒看门狗：超时转 Failed（"上传通道超时（30 秒），请重试"）并保留 pending 丢弃、下次拖拽自动重试，任何环境的静默悬挂都变成可见可重试的错误
- vid=ver-1792631491909f05，sha256 ec2db4f0…，公网 latest=Windows GUI v0.10.2，实下 sha 一致、输出 `v0.10.2 (9f792fc)`；已推送
- 等待用户复现后读 upload-debug.log 定位具体卡点

---

## 2026-10-22 发布纠错：10054 修复以 v0.10.1 发布（0.8.9/0.8.10 为错位版本号）

- **事故**：另一会话 10-17 已把版本线推到 v0.10.0（隧道管理，24224e2），本会话按陈旧记忆从 0.8.8 升版，连续发出 0.8.9、0.8.10 两个"内容含 0.10.0 全部代码但版本号更低"的错位版本，且 0.8.10 的 cleanup 删掉了服务器上的 0.10.0 条目——升级器按版本比较，装了 0.10.0 的用户永远不会"降级"到 0.8.10
- **纠正**：当前代码（隧道 + 拖拽 + 10054 修复）以 **v0.10.1** 发布：vid=ver-179262899092f23f，18.11MB，sha256 c4eed3cc…abf3；cleanup 删 0.8.10；公网 latest = Windows GUI v0.10.1 + CLI 四平台 v0.5.6；实下 sha 一致、运行输出 `v0.10.1 (6a94506)`；已推送（8591869..6a94506）
- 说明：0.8.9/0.8.10 二进制内容完整（拖拽 + 隧道都在），只是号错了；0.10.1 与 0.8.10 代码相同、仅版本号不同
- **教训（已写入长期记忆）**：升版本前必须先查当前版本线——`git log` proc.md 的最新发布记录 + /api/products 的实际 latest，而不是凭会话记忆

---

## 2026-10-14 修复（GUI）0.8.10：拖拽上传改走现有 SSH 连接的子系统通道（os error 10054）

- 用户实测：拖文件进 SSH 终端（Ubuntu），懒加载 SFTP 的新建第二条 TCP 连接被远端强制断开（os error 10054）——sshd PerSourcePenalties/MaxStartups、代理或防火墙对并发新连接的限制都会这样杀
- 架构修正：**不再开第二条连接**。SessionHandle 新增 sftp_req_tx，会话 pump 任务加 select 分支：收到请求后在 shared 已认证会话上 `channel_open_session` + `request_subsystem("sftp")` + SftpSession::new，回传 Arc<Mutex<SftpSession>>；TermUp::connect_via_session 替代 spawn_sftp 路径，UpState::Connecting 不再携带 ConnectRequest（复用已认证会话 = 无二次认证/密码弹窗），poll_tabs 的 1c 认证轮询随之删除
- 生命周期：频道随最后一个 Arc 落地自动关闭（在途上传任务持有 Arc 直到完成）；Tab 关闭 shutdown 置 None 即可；本地下 shells 的 SessionHandle 填 parking channel
- spawn_sftp/SftpClient 保留（SFTP 文件页签仍是独立连接模型，未报问题不动）
- 63 测试全过

---

## 2026-10-14 勘误（GUI）："PSReadLine 回车失效"是假警报，未修、无需发版

- 0.8.9 发布记录里的"遗留观察项"（ConPTY 重新开启 win32-input-mode 导致 PowerShell 回车失效）经对照实验**证伪**：生产代码路径（Terminal 模型经 EventProxy 自动应答 CPR）下，PowerShell 的裸回车直接可用——探针测试 `exit
` 后会话 10.2 秒内正常关闭
- 假警报根因：spike 阶段一次实验同时改了两个变量（CPR 应答时序 + 9001h 处理），把"及时应答 CPR"的功劳记在了"补发 9001l"上；后续阴性验证（禁用 fix 仍通过）暴露了矛盾
- 处置：撤销了临时的 `conpty_input_fix` 辅助函数与 app.rs 输出泵接入（未出过提交）；探针转为常驻断言测试 `conpty_enter_production_shape`（XXSSHG_LOCAL_E2E=1，守护"本地 PowerShell 回车可用"）
- 教训：一次只改一个变量；"修复"在阴性验证通过之前不算成立。0.8.9 无此问题，不需要 0.8.10

---

## 2026-10-14 发布（GUI）：v0.8.9 拖拽上传/复制到终端当前目录

- 提交顺序合规：a017833（功能+文档，先提交）→ touch main.rs 重编 → `--version` 核对 `v0.8.9 (a017833)`；55 测试全过；3622b46 补 Cargo.lock
- `publish-xxsshg-0.8.9.ps1`（0.8.8 版 sed 生成）→ vid=ver-17919631019eb9a7，17.47MB，sha256 11264080…7cdd；cleanup 脚本删除 0.8.8 旧条目，Windows GUI 仅剩 0.8.9 isLatest
- 公网验证：/api/products 显示 Windows GUI v0.8.9 isLatest=true（CLI 四平台 0.5.4 不受影响）；实下 xxsshg.exe sha 与记录一致、运行输出 `xxsshg v0.8.9 (a017833)`
- 已推送 github.com/topxeq/xxsshg（371863d..3622b46）
- 遗留观察项：生产 local.rs 只在启动时关一次 win32-input-mode，ConPTY 会重新开启（影响 PSReadLine 回车识别）——既有行为，如需修另开一项

---

## 2026-10-14 简化（GUI）：按用户决策移除 OSC 钩子注入与设置开关

- 用户定调：不加开关（增加复杂度），当前目录取不到就算了，让用户选或输入目录
- 移除：`gcfg.osc7_cwd` 设置项、设置界面行、远程会话建立时的 OSC 7 注入块、`termup::osc7_enable_bytes` 及其测试、local.rs 的 cmd/PowerShell 钩子（CMD_CWD_HOOK/PS_CWD_HOOK/`spawn` 的 cwd_hook 参数）与 osc99_gui_chain e2e（依赖注入钩子）
- 保留：OSC 7/9;9 解析与信任判据（用户自装 shell 集成时仍可直传）、全部弹框/复制/上传链路、copy_local、osc99_passthrough spike 测试（自包含，文档化 ConPTY 透传能力）、b64_utf16le（测试用）
- 行为：拖拽 → cwd 可信则直传/直复制，否则弹目录选择框（远程锚 home、本地锚 USERPROFILE）——选择框成为默认路径
- 55 测试全过，构建通过

---

## 2026-10-14 功能（GUI）：本地 cmd/PowerShell 拖拽 → 复制到终端当前目录

### 前提验证（spike，先定生死再动工）
- ConPTY 会把子进程的 OSC 9;9 序列透传到 PTY 输出流（Windows Terminal 官方 shell 集成同款机制），cmd（经 clink 展开 PROMPT）与 PowerShell 均实测通过
- 新增永久 e2e 测试 `XXSSHG_OSC99_E2E=1 cargo test osc99`：osc99_passthrough（裸 ConPTY 验证两个钩子）+ osc99_gui_chain（走生产 local::spawn + 真实 Terminal 模型，断言 trusted_cwd 初始值与 cd 跟随）

### 实现要点
- **钩子全部在 spawn 参数里**（与远程"打字注入"不同，不可见且必装）：
  - cmd：`cmd.exe /K PROMPT $E]9;9;%COMPUTERNAME%;$P$E\$P$G`（$P 每提示符求值，cd 自动跟随）
  - PowerShell：`-NoExit -EncodedCommand <b64(UTF16LE)>`，钩子为 prompt() 覆盖：先 [Console]::Write(OSC 9;9) 再输出默认样式提示符。**大坑**：`& $orig` 包装 + [Console]::Write 合在一个函数里在 ConPTY 下必挂（二分定位过），保持现形状；自定义提示符（oh-my-posh 等）在开关开启时会被替换（设置悬停说明里写明）
  - 手写 b64_utf16le（无 base64 依赖）；标准填充规则：余 1 字节 `==`、余 2 字节 `=`（写错过一版导致 PS 秒退，测试已锁定参考值）
- **term.rs 扫描器扩展**：`ESC]9;9;` 状态分支，payload `HOST;PATH`（第一个 ';' 切分；首段含盘符冒号或 UNC 开头则整体视为路径——分号合法出现在 Windows 目录名）；路径 trim 引号、必须 rooted（X:\ 或 \）；**不做百分号解码**（Windows 原生路径）。信任判据与远程一致（主机段=%COMPUTERNAME% 比对 + 回车制新鲜度）
- **termup.rs 本地模式**：TermUp::new(lang, local)，local=true 时 state 恒 Ready（无懒加载 SFTP）；upload_local → copy_local（tokio::fs 递归复制，目录合并语义，ConflictPolicy 完整复用含改名/跳过）；DirPick 本地分支（std::fs 列目录，canonicalize 后剥 `\?\` 前缀，锚点 USERPROFILE）；文案按模式分支（复制 vs 上传）
- **app.rs 本地分支多一道存在性兜底**：trusted 且 `Path::is_dir()` 才直传——这是错机器路径的最后防线（嵌套 ssh/wsl 的上报即使主机名撞车，/home/... 在本地必然 is_dir 失败 → 弹框）

### 测试与教训
- 57 测试全过（+2：osc99 扫描器 2 例、copy_local 合并/覆盖 1 例、b64 参考 1 例；gui_chain e2e 2 例环境门控）
- **e2e 教训（两个都有普遍性）**：① ConPTY 启动后会重新开启 win32-input-mode（[?9001h），此后裸 
 不再被当作回车——测试必须见 9001h 就补发 9001l（生产 local.rs 只在启动时关一次，PSReadLine 的回车受此影响是既有行为，未动）；② clink 启动发 ESC[6n 并阻塞等待应答，真实 GUI 的终端模型会自动答 CPR，模拟测试必须同样应答
- **工具教训重犯**：bash heredoc（即使 << 'EOF' 引号定界）在写含 \ 的内容时仍会吃反斜杠，本次两次踩坑（测试字面量、ps_hook 的 ST 反斜杠都被降级）——exp1.md 已有记录，此后含反斜杠的 Rust 内容一律走 Edit 工具或 python 显式 \ 字节

---

## 2026-10-14 功能（GUI）：拖拽文件进终端页签 → 上传到终端当前目录

### 需求（用户定调）
- 拖文件/文件夹进**终端页签**即上传到该终端的当前目录；目标语义 = **承载目录**（拖 `d:	mpbc` 到 `/mnt1/dest` → 得 `/mnt1/dest/abc`，与 SFTP 页 upload_path 一致）
- 当前目录**必须确定无误才直传**；否则弹目录选择对话框（定位在用户目录），可输入不存在目录并确认创建
- 不许用模糊猜测（标题解析、注入查询等一概不进直传路径）

### 设计与实现
- **可信判据**（term.rs `trusted_cwd`）：shell 亲报 OSC 7 + 主机段与页签 server.host 一致（防嵌套 ssh 传错机器；大小写不敏感；空主机段不信）+ 陈旧判定（回车制）：用户按过回车（执行了命令）后，必须等到下一个提示符的新 OSC 7 才恢复可信——收不到说明集成已死；**普通输出不使上报失效**（precmd 的 OSC 7 先于 PS1 打印，若按输出判陈旧则空闲提示符下永远不可信，自查时发现并推翻了初版"无输出制"判据）。vte 0.15 不解析 OSC 7，`feed()` 里加了独立字节状态机（Esc ] 7 ; → payload → BEL/ST，跨 chunk 安全，%XX 解码，4096 上限防失控）
- **termup.rs（新模块）**：终端页签懒加载 SFTP（`spawn_sftp` 独立连接，密码/hostkey 问题复用 app 级 Question 对话框，即 poll_tabs 1c/2c）；Ready 后后台 `canonicalize(".")` 存 home；`upload_to` 目标目录+名字拼接，复用 sftp.rs 传输原语与 ConflictPolicy（冲突对话框复刻 transfer_ask_ui）；目录选择器 DirPick（只列子目录、上级/Go/双击导航、OK 时 remote_is_dir 校验、不存在走 mkdir_all 确认创建）；进度浮层/连接徽标/结果 toast 全 painter 绘制（不抢终端事件）
- **app.rs**：Tab::Open 增加 `up: TermUp` 字段；terminal_area 拖放入队 → Idle 时 `connect()` 懒触发 → Ready+pending → trusted 直传（toast 显示目标）/ 否则 pick_open(home)；本地 shell 页签 toast 提示不支持；close_tab/reconnect_tab 均做 up.shutdown() 防连接泄漏；失败即清 pending 不自动重试（防认证失败循环）
- **可选增强（默认关）**：`gcfg.osc7_cwd` 设置项——新会话建立 800ms 后向 shell 注入单行 precmd 钩子（zsh `precmd_functions` 末位追加 / bash `PROMPT_COMMAND` 末位追加，报告 `file://<server.host>` 保证主机校验必过；嵌套机器的钩子不会继承，其自带集成若主机不符仍走对话框）。设置对话框有悬停说明

### 验证
- 51 测试全过（新增 OSC 7 解析 5 例：BEL/ST 两式、%解码/无主机段、新鲜度+主机不符、跨 chunk、非绝对路径/超长丢弃；注入片段格式 1 例）；cargo build 通过
- 待实机验证：真实服务器拖拽直传/弹框/创建目录/冲突处理/进度浮层
- 教训：bash heredoc（<<'EOF'）写 Rust 测试字面量会把 `\033` 降级成 ``（Rust 里是 NUL+"33"），断言神不知鬼不觉地错——测试字面量含反斜杠时用 Edit 工具写

---

## 2026-10-14 修复会话（GUI）：终端划选锚点偏移（egui 拖动阈值）

### 现象与用户期望
- 期望：划选起点只要落在某字符**格内**（不分左右中）就从该字符开始；终点落在某字符格内就到该字符为止
- 实际：起点常偏到隔壁字符（向右拖偏右、向左拖偏左）

### 排查链
- `point_from_pos` 自 0.1.0 起就是 `floor()`（格内即命中），`selection_text`/高亮对终点都是**闭区间**——映射语义本身正确；0.8.6 已验证过数学映射
- 字形漂移假设排除：用户机日志 `render metrics` 显示 per-char=9.164 vs cell_w=9.167（ppp=1.2），整行漂移 <0.3px，不可感知
- **真因在 egui 输入层**：`max_click_dist=6.0`——按下后指针移动超 6pt 才触发 `drag_started`；而锚点取的是**触发时刻**的 `interact_pointer_pos()`。格宽仅 9.17px，6pt≈2/3 格：按在字符 N 内拖动，drag_started 时指针常已进 N±1，锚点随之偏一格（方向随拖动方向）
- 终点无此问题：拖动中 `dragged()` 逐帧更新，release 帧也计入（`is_decidedly_dragging` 含 `any_released()`）

### 修复
- `term.rs` drag_started 分支：锚点从 `response.interact_pointer_pos()` 改为 `ui.input(|i| i.pointer.press_origin())`（egui 记录的真实按下位置），阈值漂移不再影响选区起点；shift+drag 延伸、复制跟随等行为不变
- 45 测试全过、clippy 无新警告、release 构建通过；改动未提交，随下次 GUI 发布带走

### 发布（用户实测确认后发版）
- 提交顺序合规：commit 42f8179 → touch main.rs 重编 → `--version` 核对 `v0.8.8 (42f8179)` 版本+哈希都新；45 测试全过
- `publish-xxsshg-0.8.8.ps1`（0.8.7 版 sed 生成，**新加的 cleanup-gui-latest.py 步骤首次随模板生效**，自动删除 0.8.7 旧条目）→ vid=ver-17919415103feef6，17.01MB，sha256 1e252acf…d194d
- 公网验证：Windows GUI isLatest=0.8.8（CLI 四平台 0.5.4 不受影响）；实下 xxsshg.exe sha 与记录一致、运行输出 `xxsshg v0.8.8 (42f8179)`；卡片顺序天然 GUI 在底（创建顺序，新条目最新，无需 keep-gui-bottom）
- **顺手补账**：repo-gui 的 0.8.6/0.8.7 提交一直没推远端，本次连同 0.8.8（+Cargo.lock 7b9dba2）一起 push 到 github.com/topxeq/xxsshg

## 2026-10-14 修复会话（CLI 0.5.4）：sync pull 两项 P0/P1（静默 0 传输 + --delete 删光本地）

> 本次改的是 CLI 仓库 repo/（xxssh），非 xxsshg；详见根目录 bug-report-sync-pull.md 的「修复落地」节。

### 背景（勘误）
- 前日结论「0.5.2 `sync -d pull` 引擎有 bug」**不成立**：pull 引擎正常，真因是参数顺序陷阱（put/get 的「源在前」肌肉记忆写反了 `sync <local> <remote>`）+ 「根不存在=静默空树」把错误吞成 0 传输 exit 0；`pull --delete` 下升级为静默删光本地的 P0

### 修复（repo/src：sync.rs / main.rs / i18n.rs / help.rs）
- **方向感知源根校验**：pull 要求远端根存在、push 要求本地根存在（rsync「源必须存在」）；目标端不存在仍算首次同步。缺失报 `sync_remote_root_missing`/`sync_local_root_missing`（三语）
- **写反启发式**：local 位为 `/` 开头绝对路径 + 本地不存在 + 远端位服务器上也不存在 → `sync_swapped_args`，明示「顺序固定 <local> <remote> 与方向无关」
- **空源 --delete 熔断**：源树（include/exclude 过滤后）0 文件且有计划删除 → 真跑拒绝 exit 1，提示加新旗标 `--force`；dry-run 仍显示计划。纯函数 `delete_fuse_trips` 可单测
- **诊断面**：每次 sync 输出根部统计头（两端 files/dirs 数，0 即根写错/不存在）；任一端空树打警告；`--json` 加 `warnings` 数组；`--delete` 前打「计划删除 N 个路径」高亮行；dry-run 汇总加 `（预演）` 后缀
- **非阻塞**：`ls` 人读输出首行回显 `==> path (N entries)`（打错路径回空列表不再无提示）；`--help sync` 加 ARGUMENT ORDER/MISSING ROOTS 段

### 发布（同会话完成）
- commit 8459a36 → push main + tag v0.5.4 → CI 全绿（windows/linux/macos/linux-arm64 四 job），GitHub Release v0.5.4 带全平台产物
- 仙缘渡四平台上架（publish-0.5.4{,-linux,-arm64,-mac}.ps1 由 0.5.3 版 sed 生成）：Windows 8.56MB sha c2dff8b8…（vid=ver-45020948948）；Linux x64 8.04MB sha 411c9647…（vid=ver-10902481888）；Linux ARM64 7.01MB sha eae67452…（vid=ver-26356251078）；macOS universal 18.31MB sha b11fdca2…（vid=ver-50984415910）
- Linux 两条本机 zigbuild+musl 静态交叉编译；mac universal 从 GH Release 下载覆盖 xxssh-mac-universal/ 后上架（CI macos job 自带 lipo+--version 验证）；各平台 cleanup-tui-platform.py 自动删除旧 0.5.3 条目
- 公网验证：/api/products 四平台 isLatest 均=0.5.4（Windows GUI 系列不受影响，仍 0.8.7）；downloads/xxssh/xxssh.exe 实下 sha256 与记录一致、运行输出 `xxssh 0.5.4`
- CLI 仓库无 build.rs（无 git 哈希嵌入），二进制内容与提交顺序无关；Windows 包即提交前构建的 target/release 产物

### 卡片治理（用户看到 9 行 Windows GUI 后要求处理）
- **根因**：CLI 发布脚本末尾有 `cleanup-tui-platform.py`（按平台删旧版本，只留最新），而 `publish-xxsshg-*.ps1` 从不清理——0.7.2 之后每个 GUI 版本 addVersion+isLatest=true 全部留存，卡片把 isLatest=true 的行全列出来就是 9 行 GUI
- **清理**：`python cleanup-gui-latest.py 0.8.7`（仓库现成工具，只动 Windows GUI 条目）删 8 条旧版本；副产品：≤0.5.2 老 TUI `--update` 的 GUI 污染路径同步堵死（匹配"含 windows"现在只能命中 Windows 0.5.4 本尊）
- **根治**：`publish-xxsshg-0.8.7.ps1` 在验证块前插入 `cleanup-gui-latest.py $VERSION` 调用（该脚本是下次 GUI 发布的复制模板，之后每次 GUI 发布自动清理）；PS 语法校验 738 tokens 0 errors
- **元数据**：xxssh.conf.json 更新并重推——homepageBadge v0.5.1→v0.5.4、描述顶部加「0.5.4 新增：sync 目录同步安全加固」段、GUI 版本提及 0.2.0→0.8.7；推送用 push-meta-0.5.4.ps1（登录+PUT 产品 body，与 publish 脚本第 3 步同构）
- **终态验证**：卡片版本列表恰 5 行（GUI 0.8.7 + CLI 四平台 0.5.4，全部 isLatest）、badge v0.5.4、描述头 0.5.4 段落
- **GUI 行沉底**（用户要求 GUI 排在全部 CLI 之下）：发现卡片排序 = **版本条目创建顺序**（最老在最前——GUI 0.8.7 是 09-30 的老条目所以居首）；storefront 服务端源码不在本机，无法改排序。做法：`keep-gui-bottom.py` 删掉 GUI 条目→从线上下载字节一致的原包（sha 校验 b29e7fe4…）→重建条目（新 vid=ver-55225535447）→重传→验证顺序 Windows/Linux x64/ARM64/macOS/GUI ✓，公网下载 sha 不变
- **排序保持规则**：卡片按创建顺序排，所以 **GUI 发布天然落底；CLI 发布会把新 CLI 行插到 GUI 之后（GUI 顶上去）**——每次 CLI 发布后跑一次 `python keep-gui-bottom.py [版本]`（自下载线上 GUI 包重建条目）；脚本已存 D:\aiprjs\xxssh\keep-gui-bottom.py。**同日用户取消该维护**：顺序不再管理，发布后行序随创建顺序自然排列；脚本仅留备用

### 验证
- `cargo test` 188 全过（基线 185 + 新增 3 个单测）、clippy 无新警告、release 构建通过
- aigc01 真机 7 项回归：P0 复现命令改报错、写反被识破、正确 pull 4 文件 3.1MB、增量重跑全 skip、空远端 --delete 被拒、--force 放行、ls 回显；测试残留（远端 /tmp/xxt_empty_fuse、本地临时目录）已清理

## 2026-09-30 发布会话（v0.8.7）：页签右键「复制缓冲区」+ 老 TUI 自更新污染排查

### 功能（0.8.6 → 0.8.7）
- **页签右键菜单「复制缓冲区」**：`term.rs` 新增 `buffer_text()`——从网格最顶（含回滚区）遍历到底，逐行提取文本；规则与 `selection_text()` 一致（跳过宽字符 spacer、裁行尾空格），额外丢弃缓冲区尾部空行（空闲提示符下不会复制出一屏空行）；纯读显示端网格，不进 PTY
- 菜单项在 `tabs_bar` 右键菜单「清空缓冲区」旁，仅 `Tab::Open` 显示；i18n 四语（menu_copy_buffer）
- 新增 2 个单测：30 行灌入 24 行视口（前 6 行进回滚区）断言复制内容首尾与行数；空终端返回空串。测试 43→45 全过
- 同会话答疑：回滚默认 10000 行（gconfig `default_scrollback`），设置滑块 100–100000 对数刻度；创建终端时定容量，改后只对新页签生效

### 发布
- v0.8.7 发布到仙缘渡 xxssh 卡 Windows GUI（publish-xxsshg-0.8.7.ps1）；17.01 MB，sha256 b29e7fe4…26eafb，vid=ver-43877949432，公开 API 确认 isLatest=true
- **流程补充：先提交再构建**——build.rs 把 git 短哈希嵌进二进制，未提交就构建会带旧哈希（本次首次构建即带 78ce757 旧哈希，提交 ee7347b 后重建才正确）

### 排查：老 TUI 自更新把用户的 xxssh 换成了 xxsshg
- 用户报：另一台机器升级 xxssh 后，运行却成了 xxsshg
- 逐渠道取证（全部干净）：产品 API 各版本记录的 platform/file/sha 指向正确；线上 `/downloads/xxssh/xxssh.exe` 下载后 sha 与记录吻合、运行输出 `xxssh 0.5.3`；官网 install/xxssh.ps1 的 URL+sha 校验正确；网页 CLI/GUI 下载按钮分开设；xxsshg 更新器平台过滤（windows+gui 双匹配）安全
- **真因**：≤0.5.2 的 TUI `--update` 旧逻辑 = "全平台最高 isLatest" + 平台匹配只要求"含 windows"（`"Windows GUI"` 同样命中）。用旧算法对线上数据离线模拟复现：解析出 0.8.7 → 选中 `Windows GUI v0.8.7 → xxsshg.exe` → 覆盖 xxssh.exe（sha 对着 GUI 记录校验，当然通过）。平台作用域修复 dce7b84 是 **v0.5.3 才首次带上**——与 2026-09-05 会话的"遗留"项吻合
- **处置**：受影响机器跑 `irm https://magicdo.top/install/xxssh.ps1 | iex` 重装回正确 CLI；仓库代码无需再改（0.5.3+ 已带修复）。遗留风险同旧条目：存量 ≤0.5.2 机器只要执行 --update 就会被污染

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
- 发布：改 Cargo.toml 版本 → **先 git commit（build hash 来自 HEAD，不提交则二进制带旧哈希）** → touch src/main.rs → build → 核对 `--version` 输出版本+哈希 → powershell publish-xxsshg-X.Y.Z.ps1（xxssh 根目录）
- ⚠️ 发布前必须关闭运行中的 xxsshg：exe 被占用时链接报 os error 5，**磁盘上仍是旧版**（已两次踩到）
- 诊断：XXSSHG_DEBUG_CURSOR=1（光标覆盖层+日志 ~/.xxssh/cursor-debug.log）、XXSSHG_PTY_DUMP=文件（原始字节）
- 会话诊断日志：~/.xxssh/session-debug.log（常开、UTC 时间戳；断线真因/uptime/idle/握手认证失败原因）
- 自我更新：`xxsshg.exe --update`（console 流程）或 ☰ 菜单「检查更新」；`--update` 直打真实商店可当冒烟测试
- 图标再生成：python assets/gen_icon.py（输出 xxssh-icon.png/.ico + preview*.png；调色/改字标改脚本常量）
