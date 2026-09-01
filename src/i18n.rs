//! UI internationalization for xxsshg: English / Simplified Chinese / Traditional Chinese.
//!
//! The `Language` enum and its serde representation ("en" / "zh-cn" / "zh-tw") are copied
//! from xxssh 0.5.2 `src/i18n.rs` — they share settings.json, so the format must match.
//! The translation table itself is GUI-specific.

use serde::{Deserialize, Serialize};

/// UI language (serde format shared with xxssh via settings.json)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Language {
    #[default]
    En,
    #[serde(rename = "zh-cn")]
    ZhCn,
    #[serde(rename = "zh-tw")]
    ZhTw,
}

impl Language {
    pub fn label(self) -> &'static str {
        match self {
            Language::En => "English",
            Language::ZhCn => "简体中文",
            Language::ZhTw => "繁體中文",
        }
    }

    pub fn all() -> [Language; 3] {
        [Language::En, Language::ZhCn, Language::ZhTw]
    }
}

/// Translation table: (key, en, zh-cn, zh-tw)
#[rustfmt::skip]
const T: &[(&str, &str, &str, &str)] = &[
    // ---- window / server list ----
    ("app_title", "xxsshg - SSH Client", "xxsshg - SSH 客户端", "xxsshg - SSH 用戶端"),
    ("list_title", "Servers", "服务器", "伺服器"),
    ("btn_connect", "Connect", "连接", "連線"),
    ("btn_new", "New", "新建", "新增"),
    ("btn_edit", "Edit", "编辑", "編輯"),
    ("btn_delete", "Delete", "删除", "刪除"),
    ("col_name", "Name", "名称", "名稱"),
    ("col_host", "Host", "主机", "主機"),
    ("col_user", "User", "用户", "使用者"),
    ("confirm_delete", "Delete server \"{name}\"?", "确认删除服务器「{name}」？", "確認刪除伺服器「{name}」？"),
    ("yes", "Yes", "是", "是"),
    ("no", "No", "否", "否"),
    // ---- server form ----
    ("form_new_title", "New Server", "新建服务器", "新增伺服器"),
    ("form_edit_title", "Edit Server", "编辑服务器", "編輯伺服器"),
    ("f_name", "Name", "名称", "名稱"),
    ("f_host", "Host", "主机", "主機"),
    ("f_port", "Port", "端口", "連接埠"),
    ("f_user", "Username", "用户名", "使用者名稱"),
    ("f_auth", "Auth", "认证", "認證"),
    ("auth_password", "Password", "密码", "密碼"),
    ("auth_key", "Key", "私钥", "私鑰"),
    ("f_password", "Password (empty = prompt)", "密码（留空=连接时输入）", "密碼（留空=連線時輸入）"),
    ("f_key_path", "Private key path", "私钥路径", "私鑰路徑"),
    ("f_key_pass", "Key passphrase (empty = none)", "私钥口令（留空=无）", "私鑰口令（留空=無）"),
    ("f_proxy", "SOCKS5 proxy (optional)", "SOCKS5 代理（可选）", "SOCKS5 代理（可選）"),
    ("btn_save", "Save", "保存", "儲存"),
    ("btn_cancel", "Cancel", "取消", "取消"),
    ("qc_title", "Quick Connect", "快速连接", "快速連接"),
    ("qc_connect", "Connect", "连接", "連線"),
    ("btn_close_tab", "Close", "关闭", "關閉"),
    ("menu_about", "About", "关于", "關於"),
    ("local_title", "Local", "本地终端", "本地終端"),
    ("local_cmd", "CMD", "CMD", "CMD"),
    ("local_pwsh", "PowerShell", "PowerShell", "PowerShell"),
    ("local_shell", "Shell", "Shell", "Shell"),
    ("about_text", "A lightweight GUI SSH client sharing xxssh's config.
Single binary, no runtime dependencies.

Config: ~/.xxssh (servers.json / settings.json / gui.json)", "轻量级图形 SSH 客户端，与 xxssh 共用配置。
单文件运行，无运行时依赖。

配置目录：~/.xxssh（servers.json / settings.json / gui.json）", "輕量級圖形 SSH 用戶端，與 xxssh 共用設定。
單一檔案執行，無執行時依賴。

設定目錄：~/.xxssh（servers.json / settings.json / gui.json）"),
    ("err_name_required", "Name is required", "名称不能为空", "名稱不能為空"),
    ("err_host_required", "Host is required", "主机不能为空", "主機不能為空"),
    // ---- terminal / session ----
    ("tab_new", "New terminal", "新终端", "新終端"),
    ("status_connecting", "Connecting {host}:{port}...", "正在连接 {host}:{port}...", "正在連線 {host}:{port}..."),
    ("status_connect_timeout", "Connect timeout after {secs}s: {host}", "连接超时（{secs} 秒）：{host}", "連線逾時（{secs} 秒）：{host}"),
    ("status_conn_fail", "Connection failed: {e}", "连接失败：{e}", "連線失敗：{e}"),
    ("status_proxy_fail", "Proxy error: {e}", "代理错误：{e}", "代理錯誤：{e}"),
    ("status_cancelled", "Cancelled", "已取消", "已取消"),
    ("status_connected", "Connected", "已连接", "已連線"),
    ("status_closed", "Closed ({reason})", "已断开（{reason}）", "已斷線（{reason}）"),
    ("status_auth_failed", "Auth failed: {reason}", "认证失败：{reason}", "認證失敗：{reason}"),
    ("session_enter", "Session started. Type commands here.", "会话已开始，直接输入命令。", "會話已開始，直接輸入命令。"),
    // ---- password prompt dialog ----
    ("pwd_title", "Password - {name}", "密码 - {name}", "密碼 - {name}"),
    ("pwd_prompt", "Enter password for {user}@{host}:", "请输入 {user}@{host} 的密码：", "請輸入 {user}@{host} 的密碼："),
    ("btn_ok", "OK", "确定", "確定"),
    // ---- host key dialog ----
    ("hk_title", "Host key verification", "主机指纹确认", "主機指紋確認"),
    ("hk_changed_warning", "WARNING: this host's key DIFFERS from the stored one. The connection may be intercepted, or the server was reinstalled.", "警告：该主机的密钥与已保存的不一致！连接可能被劫持，或服务器重装过。", "警告：該主機的密鑰與已儲存的不一致！連線可能被劫持，或伺服器重裝過。"),
    ("hk_unknown", "Unknown host key for {host}:{port}\n\nFingerprint (SHA256):\n{fp}\n\nTrust this host?", "首次连接 {host}:{port}，未知主机指纹：\n\nSHA256 指纹：\n{fp}\n\n是否信任该主机？", "首次連線 {host}:{port}，未知主機指紋：\n\nSHA256 指紋：\n{fp}\n\n是否信任該主機？"),
    ("hk_accept", "Trust & connect", "信任并连接", "信任並連線"),
    ("hk_reject", "Reject", "拒绝", "拒絕"),
    // ---- settings dialog ----
    ("settings_title", "Settings", "设置", "設定"),
    ("s_language", "Language", "界面语言", "介面語言"),
    ("s_theme", "Theme", "主题", "主題"),
    ("theme_dark", "Dark", "深色", "深色"),
    ("theme_light", "Light", "浅色", "淺色"),
    ("s_font_size", "Font size", "字体大小", "字體大小"),
    ("s_scrollback", "Scrollback lines", "回滚行数", "回滾行數"),
    ("s_copy_on_select", "Copy on select", "选中即复制", "選取即複製"),
    ("s_confirm_quit", "Confirm on quit", "退出时确认", "結束時確認"),
    ("s_bell", "Terminal bell", "终端铃声", "終端鈴聲"),
    ("s_invert_scroll", "Invert mouse wheel", "反转滚轮方向", "反轉滾輪方向"),
    ("s_font", "Terminal font", "终端字体", "終端字體"),
    ("hk_close_tab", "Close tab hotkey", "关闭页签热键", "關閉頁籤熱鍵"),
    ("menu_sftp", "SFTP file manager", "SFTP 文件管理", "SFTP 檔案管理"),
    ("sftp_connecting", "Opening SFTP on {host}...", "正在打开 {host} 的 SFTP...", "正在開啟 {host} 的 SFTP..."),
    ("sftp_local", "Local", "本地", "本地"),
    ("sftp_remote", "Remote", "远程", "遠端"),
    ("sftp_go", "Go", "转到", "前往"),
    ("sftp_up", "Up", "上级", "上層"),
    ("sftp_refresh", "Refresh", "刷新", "重新整理"),
    ("sftp_upload", "⬆ Upload", "⬆ 上传", "⬆ 上傳"),
    ("sftp_download", "⬇ Download", "⬇ 下载", "⬇ 下載"),
    ("sftp_new_dir", "New folder", "新建文件夹", "新增資料夾"),
    ("sftp_rename", "Rename", "重命名", "重新命名"),
    ("sftp_delete", "Delete", "删除", "刪除"),
    ("sftp_transfers", "Transfers", "传输", "傳輸"),
    ("sftp_clear", "Clear finished", "清除已完成", "清除已完成"),
    ("sftp_confirm_delete", "Delete \"{name}\"?", "确认删除 \"{name}\"？", "確認刪除 \"{name}\"？"),
    ("sftp_confirm_delete_dir", "Delete folder \"{name}\" and everything inside it?", "确认删除文件夹 \"{name}\" 及其全部内容？", "確認刪除資料夾 \"{name}\" 及其全部內容？"),
    ("sftp_new_name", "Name", "名称", "名稱"),
    // ---- sftp context menu / viewer / properties ----
    ("sftp_open", "Open", "本地打开", "本地開啟"),
    ("sftp_view", "Quick view", "快速查看", "快速檢視"),
    ("sftp_props", "Properties", "属性", "內容"),
    ("props_title", "Properties - {name}", "属性 - {name}", "內容 - {name}"),
    ("props_type", "Type", "类型", "類型"),
    ("props_type_file", "File", "文件", "檔案"),
    ("props_type_dir", "Folder", "文件夹", "資料夾"),
    ("props_location", "Location", "位置", "位置"),
    ("props_size", "Size", "大小", "大小"),
    ("props_modified", "Modified", "修改时间", "修改時間"),
    ("props_files", "{n} file(s)", "{n} 个文件", "{n} 個檔案"),
    ("props_permissions", "Permissions", "权限", "權限"),
    ("props_owner", "Owner", "所有者", "擁有者"),
    ("props_group", "Group", "属组", "群組"),
    ("props_computing", "Calculating folder size...", "正在计算文件夹大小...", "正在計算資料夾大小..."),
    ("view_title", "View - {name}", "查看 - {name}", "檢視 - {name}"),
    ("view_binary", "Binary file ({size}); text preview unavailable.", "二进制文件（{size}），无法预览文本。", "二進位檔案（{size}），無法預覽文字。"),
    ("view_truncated", "Showing the first {cap} of {total}.", "仅显示前 {cap}（共 {total}）。", "僅顯示前 {cap}（共 {total}）。"),
    ("view_wrap", "Wrap", "自动换行", "自動換行"),
    ("view_dl_cap", "File too large for quick view (over {mb} MB); download it instead.", "文件过大，无法快速查看（超过 {mb} MB），请使用下载。", "檔案過大，無法快速檢視（超過 {mb} MB），請使用下載。"),
    ("sftp_fetch_fail", "Failed to fetch \"{name}\": {e}", "获取「{name}」失败：{e}", "取得「{name}」失敗：{e}"),
    ("err_open_failed", "Failed to open: {e}", "打开失败：{e}", "開啟失敗：{e}"),
    ("hk_new_cmd", "New CMD hotkey", "新建CMD热键", "新建CMD熱鍵"),
    ("s_font_default", "Default (system)", "默认（跟随系统）", "預設（跟隨系統）"),
    ("bell_mute", "Mute", "静音", "靜音"),
    ("bell_flash", "Flash", "闪烁", "閃爍"),
    ("bell_sound", "Sound", "响铃", "響鈴"),
    ("s_saved", "Settings saved", "设置已保存", "設定已儲存"),
    // ---- quit confirm ----
    ("quit_title", "Quit xxsshg?", "退出 xxsshg？", "結束 xxsshg？"),
    ("quit_msg", "There are open sessions. Quit anyway?", "仍有打开的会话，确定退出？", "仍有開啟的會話，確定結束？"),
    ("btn_quit", "Quit", "退出", "結束"),
];

pub fn tr(lang: Language, key: &str) -> &'static str {
    for (k, en, zh_cn, zh_tw) in T {
        if *k == key {
            return match lang {
                Language::En => en,
                Language::ZhCn => zh_cn,
                Language::ZhTw => zh_tw,
            };
        }
    }
    "(?)"
}

/// Template substitution: replace `{name}`-style placeholders
pub fn tpl(template: &str, args: &[(&str, &str)]) -> String {
    let mut s = template.to_string();
    for (k, v) in args {
        s = s.replace(&format!("{{{k}}}"), v);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_serde_matches_xxssh() {
        // settings.json contract with xxssh
        assert_eq!(serde_json::to_string(&Language::En).unwrap(), "\"en\"");
        assert_eq!(serde_json::to_string(&Language::ZhCn).unwrap(), "\"zh-cn\"");
        assert_eq!(serde_json::to_string(&Language::ZhTw).unwrap(), "\"zh-tw\"");
        assert_eq!(serde_json::from_str::<Language>("\"zh-tw\"").unwrap(), Language::ZhTw);
    }

    #[test]
    fn tr_and_tpl() {
        assert_eq!(tr(Language::ZhCn, "btn_connect"), "连接");
        assert_eq!(tr(Language::En, "no_key"), "(?)");
        let s = tpl(tr(Language::ZhCn, "status_connecting"), &[("host", "1.2.3.4"), ("port", "22")]);
        assert_eq!(s, "正在连接 1.2.3.4:22...");
    }
}
