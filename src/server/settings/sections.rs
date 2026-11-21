//! The menu's sections and which one an option belongs to.

/// A tab of the settings menu, in tab order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Appearance,
    StatusLine,
    Keys,
    Layouts,
    Agents,
    Sessions,
    Remote,
    Other,
}

impl Section {
    pub const ALL: [Section; 8] = [
        Section::Appearance,
        Section::StatusLine,
        Section::Keys,
        Section::Layouts,
        Section::Agents,
        Section::Sessions,
        Section::Remote,
        Section::Other,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Section::Appearance => "Appearance",
            Section::StatusLine => "Status line",
            Section::Keys => "Keys",
            Section::Layouts => "Layouts",
            Section::Agents => "Agents",
            Section::Sessions => "Sessions and restore",
            Section::Remote => "Remote",
            Section::Other => "Other",
        }
    }
}

/// The section of option `name`, by its name.
pub fn section_of(name: &str) -> Section {
    let starts = |ps: &[&str]| ps.iter().any(|p| name.starts_with(p));
    let is = |ns: &[&str]| ns.contains(&name);
    if starts(&[
        "status",
        "window-status",
        "message",
        "display-time",
        "set-titles",
    ]) || is(&[
        "automatic-rename",
        "automatic-rename-format",
        "allow-rename",
    ]) {
        Section::StatusLine
    } else if starts(&["agent", "notify", "scratchpad"]) {
        Section::Agents
    } else if starts(&["remote-"])
        || is(&[
            "stable-ssh-agent",
            "forward-browser",
            "machine-key",
            "predictive-echo",
        ])
    {
        Section::Remote
    } else if starts(&[
        "prefix",
        "extended-keys",
        "mode-keys",
        "key-table",
        "user-keys",
    ]) || is(&[
        "escape-time",
        "repeat-time",
        "initial-repeat-time",
        "backspace",
        "mouse",
        "assume-paste-time",
        "focus-follows-mouse",
    ]) {
        Section::Keys
    } else if starts(&["main-pane", "default-layout"])
        || name.ends_with("base-index")
        || is(&[
            "default-size",
            "window-size",
            "aggressive-resize",
            "renumber-windows",
            "synchronize-panes",
        ])
    {
        Section::Layouts
    } else if starts(&["restore", "resurrect", "start-from", "exit-", "lock-"])
        || is(&[
            "history-limit",
            "destroy-unattached",
            "detach-on-destroy",
            "remain-on-exit",
            "default-command",
            "default-shell",
            "default-terminal",
        ])
    {
        Section::Sessions
    } else if name.ends_with("-style")
        || name.ends_with("-colour")
        || name.ends_with("-colours")
        || starts(&[
            "pane-border",
            "pane-active",
            "pane-frame",
            "pane-gap",
            "cursor-",
            "clock-mode",
            "copy-mode",
            "display-panes",
            "prompt-position",
            "window-style",
            "window-active",
        ])
    {
        Section::Appearance
    } else {
        Section::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn related_options_share_a_section() {
        assert_eq!(section_of("status-left"), section_of("status-right"));
        assert_ne!(section_of("status-left"), section_of("history-limit"));
        assert_eq!(section_of("pane-border-style"), Section::Appearance);
        assert_eq!(section_of("remote-reconnect"), Section::Remote);
    }
}
