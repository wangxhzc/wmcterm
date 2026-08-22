use wmcterm_config::UiLanguage;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Language {
    English,
    SimplifiedChinese,
}

impl Language {
    pub(crate) fn resolve(preference: UiLanguage) -> Self {
        match preference {
            UiLanguage::English => Self::English,
            UiLanguage::SimplifiedChinese => Self::SimplifiedChinese,
            UiLanguage::Auto => std::env::var("WMCTERM_UI_LANGUAGE")
                .ok()
                .as_deref()
                .map(|locale| Self::from_locale(Some(locale)))
                .unwrap_or_else(|| Self::from_locale(sys_locale::get_locale().as_deref())),
        }
    }

    fn from_locale(locale: Option<&str>) -> Self {
        let normalized = locale
            .unwrap_or_default()
            .split(['.', '@'])
            .next()
            .unwrap_or_default()
            .replace('_', "-")
            .to_ascii_lowercase();
        if normalized == "zh"
            || normalized.starts_with("zh-cn")
            || normalized.starts_with("zh-sg")
            || normalized.starts_with("zh-hans")
        {
            Self::SimplifiedChinese
        } else {
            Self::English
        }
    }

    pub(crate) const fn is_chinese(self) -> bool {
        matches!(self, Self::SimplifiedChinese)
    }

    pub(crate) const fn search_workspaces(self) -> &'static str {
        if self.is_chinese() {
            "搜索工作区"
        } else {
            "Search workspaces"
        }
    }

    pub(crate) const fn new_workspace(self) -> &'static str {
        if self.is_chinese() {
            "+ 新建工作区"
        } else {
            "+ New Workspace"
        }
    }

    pub(crate) const fn pinned(self) -> &'static str {
        if self.is_chinese() {
            "[已固定]"
        } else {
            "[pinned]"
        }
    }

    pub(crate) const fn find(self) -> &'static str {
        if self.is_chinese() { "查找" } else { "Find" }
    }

    pub(crate) const fn find_navigation(self) -> &'static str {
        if self.is_chinese() {
            "回车/Shift+回车"
        } else {
            "Enter/Shift+Enter"
        }
    }

    pub(crate) const fn rename_workspace(self) -> &'static str {
        if self.is_chinese() {
            "重命名工作区"
        } else {
            "RENAME WORKSPACE"
        }
    }

    pub(crate) const fn workspace_description(self) -> &'static str {
        if self.is_chinese() {
            "工作区描述"
        } else {
            "WORKSPACE DESCRIPTION"
        }
    }

    pub(crate) const fn change_workspace_directory(self) -> &'static str {
        if self.is_chinese() {
            "更改工作区目录"
        } else {
            "CHANGE WORKSPACE DIRECTORY"
        }
    }

    pub(crate) fn invalid_workspace_directory(self, detail: &str) -> String {
        if self.is_chinese() {
            format!("目录无效：{detail}")
        } else {
            format!("Invalid directory: {detail}")
        }
    }

    pub(crate) const fn find_workspace(self) -> &'static str {
        if self.is_chinese() {
            "在工作区中查找"
        } else {
            "FIND IN WORKSPACE"
        }
    }

    pub(crate) const fn filter_logs(self) -> &'static str {
        if self.is_chinese() {
            "筛选日志（level:error,warn 文本）"
        } else {
            "FILTER LOGS (level:error,warn text)"
        }
    }

    pub(crate) const fn name_required(self) -> &'static str {
        if self.is_chinese() {
            "名称不能为空"
        } else {
            "Name cannot be empty"
        }
    }

    pub(crate) const fn close_terminal_title(self) -> &'static str {
        if self.is_chinese() {
            "关闭活动终端？"
        } else {
            "CLOSE ACTIVE TERMINAL?"
        }
    }

    pub(crate) fn close_terminal_warning(
        self,
        terminal_count: usize,
        process_count: usize,
    ) -> String {
        if self.is_chinese() {
            format!(
                "将关闭 {terminal_count} 个终端，其中仍有 {process_count} 个活动进程；这些进程将被终止。"
            )
        } else {
            format!(
                "Closing {terminal_count} terminal(s) will terminate {process_count} active process(es)."
            )
        }
    }

    pub(crate) const fn close_workspace_title(self) -> &'static str {
        if self.is_chinese() {
            "关闭工作区？"
        } else {
            "CLOSE WORKSPACE?"
        }
    }

    pub(crate) fn close_workspace_warning(
        self,
        terminal_count: usize,
        process_count: usize,
    ) -> String {
        if self.is_chinese() {
            format!(
                "该工作区包含 {terminal_count} 个终端，其中共有 {process_count} 个活动进程。关闭后将无法恢复这些进程。"
            )
        } else {
            format!(
                "This workspace contains {terminal_count} terminal(s) with {process_count} active process(es). Closing it cannot restore those processes."
            )
        }
    }

    pub(crate) const fn cancel(self) -> &'static str {
        if self.is_chinese() {
            "取消"
        } else {
            "Cancel"
        }
    }

    pub(crate) const fn close_anyway(self) -> &'static str {
        if self.is_chinese() {
            "仍然关闭"
        } else {
            "Close anyway"
        }
    }

    pub(crate) const fn codex_project_trust_title(self) -> &'static str {
        if self.is_chinese() {
            "信任此 Codex 项目？"
        } else {
            "Trust this Codex project?"
        }
    }

    pub(crate) fn codex_project_trust_warning(self, path: &std::path::Path) -> String {
        if self.is_chinese() {
            format!(
                "项目：{}\n信任后，项目级 config、Hooks 和 exec policies 将在启动时生效。",
                path.display()
            )
        } else {
            format!(
                "Project: {}\nTrust enables project-local config, hooks, and exec policies at launch.",
                path.display()
            )
        }
    }

    pub(crate) fn codex_project_trust_error(self, error: &str) -> String {
        if self.is_chinese() {
            format!("配置错误：{error}")
        } else {
            format!("Configuration error: {error}")
        }
    }

    pub(crate) const fn continue_without_trust(self) -> &'static str {
        if self.is_chinese() {
            "不信任并继续"
        } else {
            "Continue restricted"
        }
    }

    pub(crate) const fn trust_and_launch(self) -> &'static str {
        if self.is_chinese() {
            "信任并启动 (T)"
        } else {
            "Trust & launch (T)"
        }
    }

    pub(crate) const fn close_application_title(self) -> &'static str {
        if self.is_chinese() {
            "关闭 wmcterm？"
        } else {
            "Close wmcterm?"
        }
    }

    pub(crate) fn close_application_warning(
        self,
        terminal_count: usize,
        process_count: usize,
        pending_request_count: usize,
    ) -> String {
        if self.is_chinese() {
            format!(
                "当前窗口的 {terminal_count} 个终端中仍有 {process_count} 个活动进程或 Agent，以及 {pending_request_count} 个待处理授权。关闭后无法恢复这些工作。"
            )
        } else {
            format!(
                "This window has {process_count} active process(es) or Agent(s) across {terminal_count} terminal(s), plus {pending_request_count} pending authorization request(s). Closing it cannot restore that work."
            )
        }
    }

    pub(crate) const fn command_palette(self) -> &'static str {
        if self.is_chinese() {
            "命令面板"
        } else {
            "COMMAND PALETTE"
        }
    }

    pub(crate) const fn needs_attention(self) -> &'static str {
        if self.is_chinese() {
            "需要处理"
        } else {
            "NEEDS ATTENTION"
        }
    }

    pub(crate) const fn mark_all_read(self) -> &'static str {
        if self.is_chinese() {
            "全部已读"
        } else {
            "Mark all read"
        }
    }

    pub(crate) const fn agent_approval(self) -> &'static str {
        if self.is_chinese() {
            "Agent 审批"
        } else {
            "AGENT APPROVAL"
        }
    }

    pub(crate) const fn details(self) -> &'static str {
        if self.is_chinese() {
            "详情"
        } else {
            "Details"
        }
    }

    pub(crate) const fn deny(self) -> &'static str {
        if self.is_chinese() { "拒绝" } else { "Deny" }
    }

    pub(crate) const fn allow_once(self) -> &'static str {
        if self.is_chinese() {
            "允许一次"
        } else {
            "Allow Once"
        }
    }

    pub(crate) const fn always_allow(self) -> &'static str {
        if self.is_chinese() {
            "信任规则"
        } else {
            "Trust Rule"
        }
    }

    pub(crate) fn full_access_confirmation(self, agent: &str) -> String {
        if self.is_chinese() {
            format!("{agent} 完全访问（危险）：输入 FULL ACCESS")
        } else {
            format!("{agent} Full Access (unsafe): type FULL ACCESS")
        }
    }

    pub(crate) const fn full_access_confirmation_error(self) -> &'static str {
        if self.is_chinese() {
            "确认短语不匹配；未启动 Agent"
        } else {
            "Confirmation phrase did not match; Agent was not started"
        }
    }

    pub(crate) fn subagent_count(self, count: usize) -> String {
        if self.is_chinese() {
            format!("{count} 个子 Agent")
        } else if count == 1 {
            "1 subagent".into()
        } else {
            format!("{count} subagents")
        }
    }

    pub(crate) const fn agent(self) -> &'static str {
        "Agent"
    }

    pub(crate) const fn idle(self) -> &'static str {
        if self.is_chinese() { "空闲" } else { "idle" }
    }

    pub(crate) const fn running(self) -> &'static str {
        if self.is_chinese() {
            "运行中"
        } else {
            "running"
        }
    }

    pub(crate) const fn waiting(self) -> &'static str {
        if self.is_chinese() {
            "等待处理"
        } else {
            "needs attention"
        }
    }

    pub(crate) const fn completed(self) -> &'static str {
        if self.is_chinese() {
            "已完成"
        } else {
            "completed"
        }
    }

    pub(crate) const fn error(self) -> &'static str {
        if self.is_chinese() { "错误" } else { "error" }
    }

    pub(crate) const fn terminal_bell(self) -> &'static str {
        if self.is_chinese() {
            "终端响铃"
        } else {
            "Terminal bell"
        }
    }

    pub(crate) fn terminal_exited(self, status: &str) -> String {
        if self.is_chinese() {
            format!("终端进程已退出（{status}） · Enter 重新连接 · Ctrl+D 关闭")
        } else {
            format!("Terminal process exited ({status}) · Enter reconnect · Ctrl+D close")
        }
    }

    pub(crate) fn terminal_error(self, error: &str) -> String {
        if self.is_chinese() {
            format!("终端错误：{error}")
        } else {
            format!("Terminal error: {error}")
        }
    }

    pub(crate) fn terminal_warning(self, warning: &str) -> String {
        if self.is_chinese() {
            format!("终端操作失败（可继续使用）：{warning}")
        } else {
            format!("Terminal operation failed (safe to continue): {warning}")
        }
    }

    pub(crate) fn terminal_spawn_failed(self, error: &str) -> String {
        if self.is_chinese() {
            format!("无法启动终端：{error}")
        } else {
            format!("Failed to start terminal: {error}")
        }
    }

    pub(crate) const fn load_surface_failed(self) -> &'static str {
        if self.is_chinese() {
            "无法加载 Surface 内容"
        } else {
            "Unable to load surface content"
        }
    }

    pub(crate) const fn browser_excluded(self) -> &'static str {
        if self.is_chinese() {
            "wmcterm 不包含内置浏览器"
        } else {
            "Browser surfaces are not included in wmcterm"
        }
    }

    pub(crate) const fn shell(self) -> &'static str {
        if self.is_chinese() { "终端" } else { "Shell" }
    }

    pub(crate) const fn empty_pane(self) -> &'static str {
        if self.is_chinese() {
            "此分区没有终端标签"
        } else {
            "This pane has no terminal tabs"
        }
    }

    pub(crate) const fn empty_workspace(self) -> &'static str {
        if self.is_chinese() {
            "当前没有工作区"
        } else {
            "There are no workspaces"
        }
    }

    pub(crate) const fn workspace_default(self) -> &'static str {
        if self.is_chinese() {
            "工作区"
        } else {
            "Workspace"
        }
    }

    pub(crate) fn action_title(self, id: &str, fallback: &'static str) -> &'static str {
        if !self.is_chinese() {
            return fallback;
        }
        match id {
            "workspace.new" => "新建工作区",
            "workspace.close" => "关闭工作区",
            "workspace.rename" => "重命名工作区",
            "workspace.describe" => "编辑工作区描述",
            "workspace.change_directory" => "更改工作区目录",
            "workspace.pin" => "固定或取消固定工作区",
            "pane.split_right" => "向右拆分",
            "pane.split_down" => "向下拆分",
            "surface.new" => "新建终端标签",
            "surface.close" => "关闭终端标签",
            "surface.filter_logs" => "筛选当前日志",
            "agent.claude" => "新建 Claude Code",
            "agent.codex" => "新建 Codex",
            "agent.claude.unattended" => "新建 Claude Code（无人值守）",
            "agent.codex.unattended" => "新建 Codex（无人值守）",
            "agent.claude.full_access" => "新建 Claude Code（完全访问…）",
            "agent.codex.full_access" => "新建 Codex（完全访问…）",
            "agent.gemini" => "新建 Gemini CLI",
            "agent.opencode" => "新建 OpenCode",
            "pane.close" => "关闭分区",
            "pane.zoom" => "切换分区缩放",
            "terminal.find" => "在终端中查找",
            "terminal.find_directory" => "在工作区文件中查找",
            "terminal.clear_scrollback" => "清除终端滚屏",
            "terminal.font_increase" => "增大字体",
            "terminal.font_decrease" => "减小字体",
            "terminal.font_reset" => "重置字体大小",
            "sidebar.toggle" => "显示或隐藏侧边栏",
            "notifications.toggle" => "显示或隐藏通知",
            "notifications.latest_unread" => "跳转到最新未读通知",
            "notifications.toggle_unread" => "切换当前通知未读状态",
            "settings.open" => "打开设置",
            "window.new" => "新建窗口",
            "app.quit" => "退出 wmcterm",
            _ => fallback,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_supported_system_locales() {
        for locale in ["zh", "zh_CN.UTF-8", "zh-Hans", "zh-SG"] {
            assert_eq!(
                Language::from_locale(Some(locale)),
                Language::SimplifiedChinese
            );
        }
        for locale in ["en_US.UTF-8", "C", "zh-TW", "zh-Hant", ""] {
            assert_eq!(Language::from_locale(Some(locale)), Language::English);
        }
    }

    #[test]
    fn translates_every_registered_action() {
        let registry = wmcterm_core::ActionRegistry::default();
        for action in registry.actions() {
            use wmcterm_core::Action;
            assert_ne!(
                Language::SimplifiedChinese.action_title(action.id(), action.title()),
                action.title(),
                "missing Chinese title for {}",
                action.id()
            );
        }
    }

    #[test]
    fn terminal_lifecycle_messages_keep_details_in_both_languages() {
        assert_eq!(
            Language::English.terminal_exited("exit code 7"),
            "Terminal process exited (exit code 7) · Enter reconnect · Ctrl+D close"
        );
        assert_eq!(
            Language::SimplifiedChinese.terminal_error("broken pipe"),
            "终端错误：broken pipe"
        );
        assert_eq!(
            Language::SimplifiedChinese.terminal_warning("copy selection: invalid value"),
            "终端操作失败（可继续使用）：copy selection: invalid value"
        );
        assert!(
            Language::SimplifiedChinese
                .terminal_spawn_failed("not found")
                .contains("not found")
        );
    }
}
