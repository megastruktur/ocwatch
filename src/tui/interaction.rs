use crate::ipc::AttachSpec;

pub fn execute_attach(attach: AttachSpec) -> Option<String> {
    match attach {
        AttachSpec::LocalTmux {
            session,
            window,
            pane,
        } => execute_tmux_attach(None, &session, window.as_deref(), pane.as_deref()),
        AttachSpec::Exec {
            program,
            args,
            tmux_window_name,
        } => execute_exec_attach(&program, &args, tmux_window_name.as_deref()),
    }
}

pub fn execute_tmux_attach(
    socket_path: Option<&str>,
    session: &str,
    window: Option<&str>,
    pane: Option<&str>,
) -> Option<String> {
    if std::env::var_os("TMUX").is_some() {
        let result = tmux_command(socket_path)
            .args(["switch-client", "-t", &tmux_target(session, window, pane)])
            .status();
        if result.map(|status| status.success()).unwrap_or(false) {
            return None;
        }

        return Some(format!(
            "tmux switch-client failed for {}",
            tmux_target(session, window, pane)
        ));
    }

    if let Some(window) = window {
        let result = tmux_command(socket_path)
            .args(["select-window", "-t", &window_target(session, window)])
            .status();

        if !result.map(|status| status.success()).unwrap_or(false) {
            return Some(format!(
                "tmux select-window failed for {}",
                window_target(session, window)
            ));
        }
    }

    if let Some(pane) = pane {
        let result = tmux_command(socket_path)
            .args(["select-pane", "-t", &pane_target(session, window, pane)])
            .status();

        if !result.map(|status| status.success()).unwrap_or(false) {
            return Some(format!(
                "tmux select-pane failed for {}",
                pane_target(session, window, pane)
            ));
        }
    }

    let result = tmux_command(socket_path)
        .args(tmux_focus_args("attach-session", session))
        .status();

    if result.map(|status| status.success()).unwrap_or(false) {
        None
    } else {
        Some(format!(
            "tmux attach-session failed for {}",
            tmux_target(session, window, pane)
        ))
    }
}

fn tmux_focus_args(command: &str, session: &str) -> Vec<String> {
    vec![command.to_string(), "-t".to_string(), session.to_string()]
}

fn tmux_target(session: &str, window: Option<&str>, pane: Option<&str>) -> String {
    match (window, pane) {
        (Some(window), Some(pane)) => format!("{}:{}.{}", session, window, pane),
        (Some(window), None) => format!("{}:{}", session, window),
        _ => session.to_string(),
    }
}

fn window_target(session: &str, window: &str) -> String {
    if window.starts_with('@') {
        window.to_string()
    } else {
        format!("{}:{}", session, window)
    }
}

fn pane_target(session: &str, window: Option<&str>, pane: &str) -> String {
    if pane.starts_with('%') {
        return pane.to_string();
    }

    match window {
        Some(window) => format!("{}:{}.{}", session, window, pane),
        None => format!("{}:.{}", session, pane),
    }
}

fn tmux_command(socket_path: Option<&str>) -> std::process::Command {
    let mut command = std::process::Command::new("tmux");
    if let Some(socket_path) = socket_path {
        command.args(["-S", socket_path]);
    }
    command
}

fn execute_exec_attach(
    program: &str,
    args: &[String],
    tmux_window_name: Option<&str>,
) -> Option<String> {
    if std::env::var_os("TMUX").is_some() {
        if let Some(window_name) = tmux_window_name {
            let shell_command = shell_join(program, args);
            let result = std::process::Command::new("tmux")
                .args(["new-window", "-n", window_name, &shell_command])
                .status();

            if result.map(|status| status.success()).unwrap_or(false) {
                return None;
            }

            return Some(format!("tmux new-window failed for {}", window_name));
        }
    }

    if let Some(window_name) = tmux_window_name {
        return execute_exec_attach_with_tmux_wrapper(program, args, window_name);
    }

    execute_direct_attach(program, args, None)
}

fn execute_exec_attach_with_tmux_wrapper(
    program: &str,
    args: &[String],
    tmux_window_name: &str,
) -> Option<String> {
    let current_exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => return Some(format!("Failed to resolve current executable: {}", error)),
    };

    let current_exe = match current_exe.to_str() {
        Some(path) => path,
        None => return Some("Current executable path is not valid UTF-8".to_string()),
    };

    let wrapper_session = format!("ocwatch-return-{}", std::process::id());
    let remote_window = sanitize_tmux_name(tmux_window_name);
    let remote_command = wrap_remote_command(&shell_join(program, args));
    let ocwatch_command = shell_escape(current_exe);

    let create_session = std::process::Command::new("tmux")
        .args([
            "new-session",
            "-d",
            "-s",
            &wrapper_session,
            "-n",
            "ocwatch",
            &ocwatch_command,
        ])
        .status();

    match create_session {
        Ok(status) if status.success() => {}
        Ok(_) | Err(_) => {
            return execute_direct_attach(
                program,
                args,
                Some("local tmux unavailable; running remote attach directly"),
            );
        }
    }

    let disable_prefix = std::process::Command::new("tmux")
        .args(["set-option", "-t", &wrapper_session, "prefix", "None"])
        .status();

    if !disable_prefix
        .map(|status| status.success())
        .unwrap_or(false)
    {
        let _ = std::process::Command::new("tmux")
            .args(["kill-session", "-t", &wrapper_session])
            .status();
        return Some(format!(
            "tmux set-option prefix failed for {}",
            wrapper_session
        ));
    }

    let disable_prefix2 = std::process::Command::new("tmux")
        .args(["set-option", "-t", &wrapper_session, "prefix2", "None"])
        .status();

    if !disable_prefix2
        .map(|status| status.success())
        .unwrap_or(false)
    {
        let _ = std::process::Command::new("tmux")
            .args(["kill-session", "-t", &wrapper_session])
            .status();
        return Some(format!(
            "tmux set-option prefix2 failed for {}",
            wrapper_session
        ));
    }

    let mut create_window_cmd = std::process::Command::new("tmux");
    create_window_cmd.args([
        "new-window",
        "-t",
        &wrapper_session,
        "-n",
        &remote_window,
        "sh",
        "-lc",
    ]);
    create_window_cmd.arg(&remote_command);
    let create_window = create_window_cmd.status();

    if !create_window
        .map(|status| status.success())
        .unwrap_or(false)
    {
        let _ = std::process::Command::new("tmux")
            .args(["kill-session", "-t", &wrapper_session])
            .status();
        return Some(format!("tmux new-window failed for {}", remote_window));
    }

    let attach_result = std::process::Command::new("tmux")
        .args([
            "attach-session",
            "-t",
            &format!("{}:{}", wrapper_session, remote_window),
        ])
        .status();

    if attach_result
        .map(|status| status.success())
        .unwrap_or(false)
    {
        None
    } else {
        let _ = std::process::Command::new("tmux")
            .args(["kill-session", "-t", &wrapper_session])
            .status();
        Some(format!(
            "tmux attach-session failed for {}",
            wrapper_session
        ))
    }
}

fn execute_direct_attach(
    program: &str,
    args: &[String],
    status_note: Option<&str>,
) -> Option<String> {
    if let Some(status_note) = status_note {
        eprintln!("{}", status_note);
    }

    let result = std::process::Command::new(program).args(args).status();
    if result.map(|status| status.success()).unwrap_or(false) {
        None
    } else {
        Some(format!("{} failed", program))
    }
}

fn shell_join(program: &str, args: &[String]) -> String {
    let mut parts = Vec::with_capacity(args.len() + 1);
    parts.push(shell_escape(program));
    parts.extend(args.iter().map(|arg| shell_escape(arg)));
    parts.join(" ")
}

fn shell_escape(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }

    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':' | '@'))
    {
        return value.to_string();
    }

    format!("'{}'", value.replace('\'', "'\\''"))
}

fn sanitize_tmux_name(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => ch,
            _ => '-',
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();

    if sanitized.is_empty() {
        "remote".to_string()
    } else {
        sanitized
    }
}

fn wrap_remote_command(command: &str) -> String {
    format!(
        "{command}; status=$?; if [ $status -ne 0 ]; then printf '\\nremote drop-in exited with status %s\\n' \"$status\"; sleep 5; fi; exit $status",
        command = command,
    )
}
