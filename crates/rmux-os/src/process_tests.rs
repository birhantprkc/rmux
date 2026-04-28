use super::*;

#[test]
fn fd_path_rejects_negative_descriptors() {
    assert_eq!(fd_path(std::process::id(), -1), None);
}

#[test]
fn current_process_is_live() {
    assert_eq!(
        ProcessInspector
            .is_live(std::process::id())
            .expect("liveness query"),
        Some(true)
    );
    assert!(is_live(std::process::id()));
}

#[cfg(unix)]
#[test]
fn current_process_path_is_available() {
    let path = current_path(std::process::id()).expect("current process cwd should be visible");
    assert!(!path.is_empty());
}

#[test]
fn current_process_command_name_is_available() {
    let name = command_name(std::process::id()).expect("current process command should be visible");
    assert!(!name.is_empty());
}

#[test]
#[cfg(unix)]
fn current_process_environment_is_available() {
    let environment =
        environment(std::process::id()).expect("current process environment should be visible");
    assert!(!environment.is_empty());
}

#[cfg(windows)]
#[test]
fn windows_reports_exited_process_as_dead_even_with_exit_code_259() {
    let mut child = std::process::Command::new("cmd.exe")
        .args(["/C", "exit", "259"])
        .spawn()
        .expect("spawn exit-code helper");
    let pid = child.id();

    loop {
        if child.try_wait().expect("poll helper").is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    assert_eq!(
        ProcessInspector.is_live(pid).expect("liveness query"),
        Some(false)
    );
}

#[cfg(windows)]
#[test]
fn windows_reports_unavailable_environment_as_ok_none() {
    assert_eq!(
        ProcessInspector
            .environment(std::process::id())
            .expect("environment query should not fail"),
        None
    );
}

#[cfg(windows)]
#[test]
fn windows_reports_unavailable_fd_path_as_ok_none() {
    assert_eq!(
        ProcessInspector
            .fd_path(std::process::id(), 0)
            .expect("fd path query should not fail"),
        None
    );
}

#[test]
fn parses_nul_separated_environment() {
    let environment = environment_from_nul_entries(b"A=1\0B=two\0\0").expect("environment");

    assert_eq!(environment.get("A").map(String::as_str), Some("1"));
    assert_eq!(environment.get("B").map(String::as_str), Some("two"));
}

#[cfg(target_os = "macos")]
#[test]
fn parses_macos_procargs_environment() {
    let mut buffer = Vec::new();
    let argc: libc::c_int = 2;
    buffer.extend_from_slice(&argc.to_ne_bytes());
    buffer.extend_from_slice(b"/bin/zsh\0");
    buffer.extend_from_slice(b"\0\0");
    buffer.extend_from_slice(b"zsh\0-l\0");
    buffer.extend_from_slice(b"RMUX_PANE=%1\0LANG=en_US.UTF-8\0\0");

    let environment = environment_from_macos_procargs(&buffer).expect("environment");

    assert_eq!(environment.get("RMUX_PANE").map(String::as_str), Some("%1"));
    assert_eq!(
        environment.get("LANG").map(String::as_str),
        Some("en_US.UTF-8")
    );
}
