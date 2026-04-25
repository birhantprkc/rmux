mod common;

use std::error::Error;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use common::{
    CapturedCommand, EnvironmentOverrides, FrozenTmuxBinary, TmuxCompatHarness, TmuxCompatRun,
    TmuxCompatRunConfig, FROZEN_TMUX_ENV,
};
use rmux_core::{input::InputParser, Screen};
use rmux_proto::TerminalSize as ScreenTerminalSize;
use rmux_pty::{PtyPair, TerminalSize as PtyTerminalSize};
const TMUX_COMPAT_TIMEOUT: Duration = Duration::from_secs(3);

#[test]
fn tmux_compat_explicit_config_layout_and_format_surfaces_when_frozen_tmux_is_available(
) -> Result<(), Box<dyn Error>> {
    let harness = TmuxCompatHarness::new("tmux-compat-config-layout-format")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let (config, expected_overrides) = config_with_clean_homes(&harness)?;
    let config_path = harness.tmpdir().join("startup.conf");
    fs::write(
        &config_path,
        "set-option -g status off\nset-option -g status-left '#{session_name}'\n",
    )?;
    let config_path = config_path.to_string_lossy().into_owned();

    let create = harness.run_pair_with(
        &tmux_binary,
        &[
            "-f",
            &config_path,
            "new-session",
            "-d",
            "-s",
            "alpha",
            "-x",
            "80",
            "-y",
            "24",
        ],
        config.clone(),
    )?;
    assert_quiet_success(&create);
    assert_run_metadata(
        &create,
        &harness,
        &tmux_binary,
        &[
            "-f",
            &config_path,
            "new-session",
            "-d",
            "-s",
            "alpha",
            "-x",
            "80",
            "-y",
            "24",
        ],
        &expected_overrides,
    );

    let split = harness.run_pair_with(
        &tmux_binary,
        &["split-window", "-h", "-t", "alpha"],
        config.clone(),
    )?;
    assert_quiet_success(&split);
    assert_run_metadata(
        &split,
        &harness,
        &tmux_binary,
        &["split-window", "-h", "-t", "alpha"],
        &expected_overrides,
    );

    let split_layout = harness.run_pair_with(
        &tmux_binary,
        &["lsw", "-t", "alpha", "-F", "#{window_layout}"],
        config.clone(),
    )?;
    assert_exact_tmux_compat(&split_layout);
    assert_run_metadata(
        &split_layout,
        &harness,
        &tmux_binary,
        &["lsw", "-t", "alpha", "-F", "#{window_layout}"],
        &expected_overrides,
    );

    let select_layout = harness.run_pair_with(
        &tmux_binary,
        &["select-layout", "-t", "alpha:0", "even-horizontal"],
        config.clone(),
    )?;
    assert_quiet_success(&select_layout);
    assert_run_metadata(
        &select_layout,
        &harness,
        &tmux_binary,
        &["select-layout", "-t", "alpha:0", "even-horizontal"],
        &expected_overrides,
    );

    let list_windows = harness.run_pair_with(
        &tmux_binary,
        &["lsw", "-t", "alpha", "-F", "#{window_layout}"],
        config.clone(),
    )?;
    // Frozen tmux next-3.7 keeps the explicit split geometry here; RMUX follows
    // the system tmux 3.4 behavior observed in the interactive Mate-terminal
    // check, where explicit even-horizontal recomputes balanced pane widths.
    assert_eq!(list_windows.tmux.status_code, list_windows.rmux.status_code);
    assert_eq!(list_windows.tmux.timed_out, list_windows.rmux.timed_out);
    assert_eq!(list_windows.tmux.stderr, list_windows.rmux.stderr);
    assert_eq!(
        list_windows.rmux.stdout_string(),
        "89f5,80x24,0,0{39x24,0,0,0,40x24,40,0,1}\n"
    );
    assert_eq!(
        list_windows.tmux.stdout_string(),
        "8205,80x24,0,0{40x24,0,0,0,39x24,41,0,1}\n"
    );
    assert_run_metadata(
        &list_windows,
        &harness,
        &tmux_binary,
        &["lsw", "-t", "alpha", "-F", "#{window_layout}"],
        &expected_overrides,
    );

    let display = harness.run_pair_with(
        &tmux_binary,
        &[
            "display-message",
            "-p",
            "-t",
            "alpha:0.0",
            "#{E:status-left}:#{window_layout}:#{?#{==:#{session_name},alpha},#{=5:abcdefgh},no}",
        ],
        config,
    )?;
    // Keep the same version-specific layout assertion for the format expansion
    // path so status-left and conditional formatting remain covered.
    assert_eq!(display.tmux.status_code, display.rmux.status_code);
    assert_eq!(display.tmux.timed_out, display.rmux.timed_out);
    assert_eq!(display.tmux.stderr, display.rmux.stderr);
    assert_eq!(
        display.rmux.stdout_string(),
        "alpha:89f5,80x24,0,0{39x24,0,0,0,40x24,40,0,1}:\n"
    );
    assert_eq!(
        display.tmux.stdout_string(),
        "alpha:8205,80x24,0,0{40x24,0,0,0,39x24,41,0,1}:\n"
    );
    assert_run_metadata(
        &display,
        &harness,
        &tmux_binary,
        &[
            "display-message",
            "-p",
            "-t",
            "alpha:0.0",
            "#{E:status-left}:#{window_layout}:#{?#{==:#{session_name},alpha},#{=5:abcdefgh},no}",
        ],
        &expected_overrides,
    );
    assert!(
        display.rmux.stdout_string().starts_with("alpha:"),
        "expected display-message output to reflect config-loaded formats, got {:?}",
        display.rmux.stdout_string()
    );

    Ok(())
}

#[test]
fn tmux_compat_explicit_missing_config_file_is_silent_for_detached_new_session_when_frozen_tmux_is_available(
) -> Result<(), Box<dyn Error>> {
    let harness = TmuxCompatHarness::new("tmux-compat-config-missing-detached")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let (config, expected_overrides) = config_with_clean_homes(&harness)?;
    let missing_config = harness.tmpdir().join("nonexistent.conf");
    let missing_config = missing_config.to_string_lossy().into_owned();
    let argv = [
        "-f",
        missing_config.as_str(),
        "new-session",
        "-d",
        "-s",
        "alpha",
        "-x",
        "80",
        "-y",
        "24",
    ];

    let create = harness.run_pair_with(&tmux_binary, &argv, config)?;

    assert_run_metadata(&create, &harness, &tmux_binary, &argv, &expected_overrides);
    assert_exact_tmux_compat(&create);
    assert_eq!(create.tmux.status_code, Some(0));
    assert!(create.tmux.stdout.is_empty());
    assert!(create.tmux.stderr.is_empty());
    Ok(())
}

#[test]
fn tmux_compat_environment_style_and_terminal_feature_lines_when_frozen_tmux_is_available(
) -> Result<(), Box<dyn Error>> {
    let harness = TmuxCompatHarness::new("tmux-compat-env-style-terminal-features")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let config = tmux_compat_config();
    let expected_overrides = default_overrides(harness.tmpdir());

    let create = harness.run_pair_with(
        &tmux_binary,
        &["new-session", "-d", "-s", "alpha"],
        config.clone(),
    )?;
    assert_quiet_success(&create);
    assert_run_metadata(
        &create,
        &harness,
        &tmux_binary,
        &["new-session", "-d", "-s", "alpha"],
        &expected_overrides,
    );

    let set_environment = harness.run_pair_with(
        &tmux_binary,
        &["setenv", "-t", "alpha", "TERM", "screen"],
        config.clone(),
    )?;
    assert_quiet_success(&set_environment);
    assert_run_metadata(
        &set_environment,
        &harness,
        &tmux_binary,
        &["setenv", "-t", "alpha", "TERM", "screen"],
        &expected_overrides,
    );

    let show_environment =
        harness.run_pair_with(&tmux_binary, &["showenv", "-t", "alpha"], config.clone())?;
    assert_success_without_stderr(&show_environment);
    assert_run_metadata(
        &show_environment,
        &harness,
        &tmux_binary,
        &["showenv", "-t", "alpha"],
        &expected_overrides,
    );
    let tmux_showenv = show_environment.tmux.stdout_string();
    let rmux_showenv = show_environment.rmux.stdout_string();
    assert!(
        tmux_showenv.contains("TERM=screen\n"),
        "expected tmux showenv output to include TERM override, got {tmux_showenv:?}"
    );
    assert_eq!(rmux_showenv, tmux_showenv);

    let set_terminal_features = harness.run_pair_with(
        &tmux_binary,
        &["set", "-as", "terminal-features", ",xterm-256color:RGB"],
        config.clone(),
    )?;
    assert_quiet_success(&set_terminal_features);
    assert_run_metadata(
        &set_terminal_features,
        &harness,
        &tmux_binary,
        &["set", "-as", "terminal-features", ",xterm-256color:RGB"],
        &expected_overrides,
    );

    let set_status_style = harness.run_pair_with(
        &tmux_binary,
        &["set", "-g", "status-style", "bg=green,fg=black"],
        config.clone(),
    )?;
    assert_quiet_success(&set_status_style);
    assert_run_metadata(
        &set_status_style,
        &harness,
        &tmux_binary,
        &["set", "-g", "status-style", "bg=green,fg=black"],
        &expected_overrides,
    );

    let show_options =
        harness.run_pair_with(&tmux_binary, &["show-options", "-g"], config.clone())?;
    assert_run_metadata(
        &show_options,
        &harness,
        &tmux_binary,
        &["show-options", "-g"],
        &expected_overrides,
    );
    assert_success_without_stderr(&show_options);

    let status_style_line = assert_matching_line(&show_options, "status-style ");
    assert_eq!(status_style_line, "status-style bg=green,fg=black");

    let rmux_server_terminal_features =
        harness.run_rmux(&["show-options", "-sv", "terminal-features"])?;
    assert_eq!(rmux_server_terminal_features.status_code, Some(0));
    assert!(!rmux_server_terminal_features.timed_out);
    assert!(rmux_server_terminal_features.stderr_string().is_empty());
    assert!(
        rmux_server_terminal_features
            .stdout_string()
            .contains("RGB"),
        "expected rmux server-scope terminal-features query to record RGB support, got {:?}",
        rmux_server_terminal_features.stdout_string()
    );

    Ok(())
}

#[test]
fn tmux_compat_window_option_alias_surface_when_frozen_tmux_is_available(
) -> Result<(), Box<dyn Error>> {
    let harness = TmuxCompatHarness::new("tmux-compat-window-option-alias-surface")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let config = tmux_compat_config();
    let expected_overrides = default_overrides(harness.tmpdir());

    let create = harness.run_pair_with(
        &tmux_binary,
        &["new-session", "-d", "-s", "alpha"],
        config.clone(),
    )?;
    assert_quiet_success(&create);
    assert_run_metadata(
        &create,
        &harness,
        &tmux_binary,
        &["new-session", "-d", "-s", "alpha"],
        &expected_overrides,
    );

    let set_window_toggle = harness.run_pair_with(
        &tmux_binary,
        &["set-option", "-w", "-t", "alpha", "synchronize-panes"],
        config.clone(),
    )?;
    assert_exact_tmux_compat(&set_window_toggle);
    assert_run_metadata(
        &set_window_toggle,
        &harness,
        &tmux_binary,
        &["set-option", "-w", "-t", "alpha", "synchronize-panes"],
        &expected_overrides,
    );

    let show_window_toggle = harness.run_pair_with(
        &tmux_binary,
        &["show-options", "-wv", "-t", "alpha", "synchronize-panes"],
        config.clone(),
    )?;
    assert_exact_tmux_compat(&show_window_toggle);
    assert_run_metadata(
        &show_window_toggle,
        &harness,
        &tmux_binary,
        &["show-options", "-wv", "-t", "alpha", "synchronize-panes"],
        &expected_overrides,
    );
    assert_eq!(show_window_toggle.rmux.stdout_string(), "on\n");

    let set_window_option = harness.run_pair_with(
        &tmux_binary,
        &["setw", "-t", "alpha", "pane-border-style", "fg=colour1"],
        config.clone(),
    )?;
    assert_exact_tmux_compat(&set_window_option);
    assert_run_metadata(
        &set_window_option,
        &harness,
        &tmux_binary,
        &["setw", "-t", "alpha", "pane-border-style", "fg=colour1"],
        &expected_overrides,
    );

    let show_window_option = harness.run_pair_with(
        &tmux_binary,
        &["showw", "-v", "-t", "alpha", "pane-border-style"],
        config.clone(),
    )?;
    assert_exact_tmux_compat(&show_window_option);
    assert_run_metadata(
        &show_window_option,
        &harness,
        &tmux_binary,
        &["showw", "-v", "-t", "alpha", "pane-border-style"],
        &expected_overrides,
    );
    assert_eq!(show_window_option.rmux.stdout_string(), "fg=colour1\n");

    let set_window_option_full = harness.run_pair_with(
        &tmux_binary,
        &[
            "set-window-option",
            "-t",
            "alpha",
            "pane-border-style",
            "fg=colour2",
        ],
        config.clone(),
    )?;
    assert_exact_tmux_compat(&set_window_option_full);
    assert_run_metadata(
        &set_window_option_full,
        &harness,
        &tmux_binary,
        &[
            "set-window-option",
            "-t",
            "alpha",
            "pane-border-style",
            "fg=colour2",
        ],
        &expected_overrides,
    );

    let show_window_option_full = harness.run_pair_with(
        &tmux_binary,
        &[
            "show-window-options",
            "-v",
            "-t",
            "alpha",
            "pane-border-style",
        ],
        config.clone(),
    )?;
    assert_exact_tmux_compat(&show_window_option_full);
    assert_run_metadata(
        &show_window_option_full,
        &harness,
        &tmux_binary,
        &[
            "show-window-options",
            "-v",
            "-t",
            "alpha",
            "pane-border-style",
        ],
        &expected_overrides,
    );
    assert_eq!(show_window_option_full.rmux.stdout_string(), "fg=colour2\n");

    let set_server_message_limit = harness.run_pair_with(
        &tmux_binary,
        &["set-option", "-s", "message-limit", "77"],
        config.clone(),
    )?;
    assert_exact_tmux_compat(&set_server_message_limit);
    assert_run_metadata(
        &set_server_message_limit,
        &harness,
        &tmux_binary,
        &["set-option", "-s", "message-limit", "77"],
        &expected_overrides,
    );

    let show_server_message_limit = harness.run_pair_with(
        &tmux_binary,
        &["show-options", "-gsv", "-t", "missing", "message-limit"],
        config.clone(),
    )?;
    assert_exact_tmux_compat(&show_server_message_limit);
    assert_run_metadata(
        &show_server_message_limit,
        &harness,
        &tmux_binary,
        &["show-options", "-gsv", "-t", "missing", "message-limit"],
        &expected_overrides,
    );
    assert_eq!(show_server_message_limit.rmux.stdout_string(), "77\n");

    let set_window_option_global = harness.run_pair_with(
        &tmux_binary,
        &["set-window-option", "-g", "pane-border-style", "fg=colour3"],
        config.clone(),
    )?;
    assert_exact_tmux_compat(&set_window_option_global);
    assert_run_metadata(
        &set_window_option_global,
        &harness,
        &tmux_binary,
        &["set-window-option", "-g", "pane-border-style", "fg=colour3"],
        &expected_overrides,
    );

    let show_window_option_global = harness.run_pair_with(
        &tmux_binary,
        &[
            "show-window-options",
            "-g",
            "-t",
            "missing",
            "-v",
            "pane-border-style",
        ],
        config,
    )?;
    assert_exact_tmux_compat(&show_window_option_global);
    assert_run_metadata(
        &show_window_option_global,
        &harness,
        &tmux_binary,
        &[
            "show-window-options",
            "-g",
            "-t",
            "missing",
            "-v",
            "pane-border-style",
        ],
        &expected_overrides,
    );
    assert_eq!(
        show_window_option_global.rmux.stdout_string(),
        "fg=colour3\n"
    );

    Ok(())
}

#[test]
fn tmux_compat_hook_arrays_aliases_and_exact_targets_when_frozen_tmux_is_available(
) -> Result<(), Box<dyn Error>> {
    let harness = TmuxCompatHarness::new("tmux-compat-hooks-aliases-targets")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let config = tmux_compat_config();
    let expected_overrides = default_overrides(harness.tmpdir());

    let bootstrap = harness.run_pair_with(
        &tmux_binary,
        &["new-session", "-d", "-s", "bootstrap"],
        config.clone(),
    )?;
    assert_quiet_success(&bootstrap);
    assert_run_metadata(
        &bootstrap,
        &harness,
        &tmux_binary,
        &["new-session", "-d", "-s", "bootstrap"],
        &expected_overrides,
    );

    let hook_first = harness.run_pair_with(
        &tmux_binary,
        &[
            "set-hook",
            "-ag",
            "session-created",
            "set-buffer -b first first",
        ],
        config.clone(),
    )?;
    assert_quiet_success(&hook_first);
    assert_run_metadata(
        &hook_first,
        &harness,
        &tmux_binary,
        &[
            "set-hook",
            "-ag",
            "session-created",
            "set-buffer -b first first",
        ],
        &expected_overrides,
    );

    let hook_second = harness.run_pair_with(
        &tmux_binary,
        &[
            "set-hook",
            "-ag",
            "session-created",
            "set-buffer -b second second",
        ],
        config.clone(),
    )?;
    assert_quiet_success(&hook_second);
    assert_run_metadata(
        &hook_second,
        &harness,
        &tmux_binary,
        &[
            "set-hook",
            "-ag",
            "session-created",
            "set-buffer -b second second",
        ],
        &expected_overrides,
    );

    let create = harness.run_pair_with(
        &tmux_binary,
        &["new", "-d", "-s", "alpha", "-x", "80", "-y", "24"],
        config.clone(),
    )?;
    assert_quiet_success(&create);
    assert_run_metadata(
        &create,
        &harness,
        &tmux_binary,
        &["new", "-d", "-s", "alpha", "-x", "80", "-y", "24"],
        &expected_overrides,
    );

    let first_buffer =
        harness.run_pair_with(&tmux_binary, &["showb", "-b", "first"], config.clone())?;
    assert_exact_tmux_compat(&first_buffer);
    assert_run_metadata(
        &first_buffer,
        &harness,
        &tmux_binary,
        &["showb", "-b", "first"],
        &expected_overrides,
    );
    assert_eq!(first_buffer.rmux.stdout_string(), "first");

    let second_buffer =
        harness.run_pair_with(&tmux_binary, &["showb", "-b", "second"], config.clone())?;
    assert_exact_tmux_compat(&second_buffer);
    assert_run_metadata(
        &second_buffer,
        &harness,
        &tmux_binary,
        &["showb", "-b", "second"],
        &expected_overrides,
    );
    assert_eq!(second_buffer.rmux.stdout_string(), "second");

    let display = harness.run_pair_with(
        &tmux_binary,
        &[
            "display",
            "-p",
            "-t",
            "alpha:0.0",
            "#{session_name}:#{pane_id}:#{pane_width}x#{pane_height}:#{window_panes}:#{pane_active}",
        ],
        config,
    )?;
    assert_success_without_stderr(&display);
    assert_run_metadata(
        &display,
        &harness,
        &tmux_binary,
        &[
            "display",
            "-p",
            "-t",
            "alpha:0.0",
            "#{session_name}:#{pane_id}:#{pane_width}x#{pane_height}:#{window_panes}:#{pane_active}",
        ],
        &expected_overrides,
    );
    assert!(
        display.tmux.stdout_string().starts_with("alpha:%"),
        "expected tmux display alias output to include an exact pane target, got {:?}",
        display.tmux.stdout_string()
    );
    assert!(
        display.rmux.stdout_string().starts_with("alpha:%"),
        "expected display alias output to include an exact pane target and pane id, got {:?}",
        display.rmux.stdout_string()
    );
    assert_exact_tmux_compat(&display);
    Ok(())
}

fn frozen_tmux_or_skip(harness: &TmuxCompatHarness) -> Result<Option<PathBuf>, Box<dyn Error>> {
    match FrozenTmuxBinary::discover() {
        FrozenTmuxBinary::Available(path) => Ok(Some(path)),
        FrozenTmuxBinary::Unavailable {
            checked_path,
            reason,
        } => {
            eprintln!(
                "runtime skip: frozen tmux binary unavailable via {FROZEN_TMUX_ENV} or default '{}': {reason}",
                checked_path.display()
            );
            harness.assert_socket_dirs_clean()?;
            Ok(None)
        }
    }
}

fn tmux_compat_config() -> TmuxCompatRunConfig {
    TmuxCompatRunConfig::default().with_timeout(TMUX_COMPAT_TIMEOUT)
}

fn config_with_clean_homes(
    harness: &TmuxCompatHarness,
) -> Result<(TmuxCompatRunConfig, EnvironmentOverrides), Box<dyn Error>> {
    let home = harness.tmpdir().join("home");
    let xdg = harness.tmpdir().join("xdg");
    fs::create_dir_all(&home)?;
    fs::create_dir_all(&xdg)?;

    let config = tmux_compat_config()
        .with_env("HOME", home.as_os_str())
        .with_env("XDG_CONFIG_HOME", xdg.as_os_str());
    let overrides = default_overrides(harness.tmpdir())
        .into_iter()
        .chain([
            (OsString::from("HOME"), Some(home.as_os_str().to_owned())),
            (
                OsString::from("XDG_CONFIG_HOME"),
                Some(xdg.as_os_str().to_owned()),
            ),
        ])
        .collect();
    Ok((config, overrides))
}

fn default_overrides(tmpdir: &Path) -> EnvironmentOverrides {
    vec![
        (
            OsString::from("TMPDIR"),
            Some(tmpdir.as_os_str().to_owned()),
        ),
        (
            OsString::from("TMUX_TMPDIR"),
            Some(tmpdir.as_os_str().to_owned()),
        ),
        (OsString::from("TMUX"), None),
        (
            OsString::from("TERM"),
            Some(OsString::from("xterm-256color")),
        ),
    ]
}

fn assert_run_metadata(
    run: &TmuxCompatRun,
    harness: &TmuxCompatHarness,
    tmux_binary: &Path,
    argv: &[&str],
    expected_overrides: &EnvironmentOverrides,
) {
    assert_command_metadata(
        &run.rmux,
        "rmux",
        Path::new(env!("CARGO_BIN_EXE_rmux")),
        harness.rmux_socket_dir(),
        argv,
        &argv_os(argv),
        expected_overrides,
    );
    assert_command_metadata(
        &run.tmux,
        "tmux",
        tmux_binary,
        harness.tmux_socket_dir(),
        argv,
        &tmux_effective_argv(harness, argv),
        expected_overrides,
    );
}

fn assert_rmux_metadata(
    command: &CapturedCommand,
    harness: &TmuxCompatHarness,
    argv: &[&str],
    expected_overrides: &EnvironmentOverrides,
) {
    assert_command_metadata(
        command,
        "rmux",
        Path::new(env!("CARGO_BIN_EXE_rmux")),
        harness.rmux_socket_dir(),
        argv,
        &argv_os(argv),
        expected_overrides,
    );
}

fn assert_command_metadata(
    command: &CapturedCommand,
    program: &str,
    program_path: &Path,
    socket_dir: &Path,
    requested_argv: &[&str],
    effective_argv: &[OsString],
    expected_overrides: &EnvironmentOverrides,
) {
    assert_eq!(command.program, program);
    assert_eq!(command.program_path, program_path);
    assert_eq!(command.requested_argv, argv_os(requested_argv));
    assert_eq!(command.effective_argv, effective_argv);
    assert_eq!(command.socket_dir, socket_dir);
    assert_eq!(command.timeout, TMUX_COMPAT_TIMEOUT);
    assert_eq!(&command.environment_overrides, expected_overrides);
}

fn tmux_effective_argv(harness: &TmuxCompatHarness, argv: &[&str]) -> Vec<OsString> {
    let mut effective = vec![
        OsString::from("-S"),
        harness.tmux_socket_path().as_os_str().to_owned(),
    ];
    effective.extend(argv_os(argv));
    effective
}

fn argv_os(argv: &[&str]) -> Vec<OsString> {
    argv.iter().map(OsString::from).collect()
}

fn assert_quiet_success(run: &TmuxCompatRun) {
    assert_eq!(
        run.tmux.status_code,
        Some(0),
        "tmux failed: stdout={:?} stderr={:?}",
        run.tmux.stdout_string(),
        run.tmux.stderr_string()
    );
    assert_eq!(
        run.rmux.status_code,
        Some(0),
        "rmux failed: stdout={:?} stderr={:?}",
        run.rmux.stdout_string(),
        run.rmux.stderr_string()
    );
    assert!(!run.tmux.timed_out);
    assert!(!run.rmux.timed_out);
    assert!(
        run.tmux.stdout.is_empty(),
        "tmux stdout should be empty, got {:?}",
        run.tmux.stdout_string()
    );
    assert!(
        run.rmux.stdout.is_empty(),
        "rmux stdout should be empty, got {:?}",
        run.rmux.stdout_string()
    );
    assert!(
        run.tmux.stderr.is_empty(),
        "tmux stderr should be empty, got {:?}",
        run.tmux.stderr_string()
    );
    assert!(
        run.rmux.stderr.is_empty(),
        "rmux stderr should be empty, got {:?}",
        run.rmux.stderr_string()
    );
}

fn assert_exact_tmux_compat(run: &TmuxCompatRun) {
    assert_eq!(run.tmux.status_code, run.rmux.status_code);
    assert_eq!(run.tmux.timed_out, run.rmux.timed_out);
    assert_eq!(run.tmux.stdout, run.rmux.stdout);
    assert_eq!(run.tmux.stderr, run.rmux.stderr);
}

fn drop_frozen_mirrored_layout_bindings(output: &[u8]) -> Vec<u8> {
    let output = std::str::from_utf8(output).expect("list-keys output is UTF-8");
    let mut normalized = String::new();
    for line in output.lines() {
        if matches!(line, "prefix:M-6:0" | "prefix:M-7:0") {
            continue;
        }
        normalized.push_str(line);
        normalized.push('\n');
    }
    normalized.into_bytes()
}

fn collapse_repeated_horizontal_borders(line: &str) -> String {
    let mut collapsed = String::with_capacity(line.len());
    let mut previous_horizontal = false;
    for character in line.chars() {
        if character == '─' {
            if !previous_horizontal {
                collapsed.push(character);
            }
            previous_horizontal = true;
        } else {
            collapsed.push(character);
            previous_horizontal = false;
        }
    }
    collapsed
}

fn utf8_window_name_display_is_ready(output: &str) -> bool {
    let Some(output) = output.strip_suffix('\n') else {
        return false;
    };
    let mut parts = output.split(':');
    matches!(
        (parts.next(), parts.next(), parts.next(), parts.next()),
        (Some("alpha"), Some(window_name), Some("80"), None) if !window_name.is_empty()
    )
}

fn assert_success_without_stderr(run: &TmuxCompatRun) {
    assert_eq!(run.tmux.status_code, Some(0));
    assert_eq!(run.rmux.status_code, Some(0));
    assert!(!run.tmux.timed_out);
    assert!(!run.rmux.timed_out);
    assert!(run.tmux.stderr_string().is_empty());
    assert!(run.rmux.stderr_string().is_empty());
}

#[derive(Debug)]
struct PtyAttachedClient {
    master: File,
    child: Child,
}

impl PtyAttachedClient {
    fn spawn(mut command: Command) -> Result<Self, Box<dyn Error>> {
        let pty = PtyPair::open_with_size(PtyTerminalSize { cols: 80, rows: 24 })?;
        let master = File::from(pty.master().try_clone()?.into_owned_fd());
        let _terminal = File::from(pty.slave().try_clone()?.into_owned_fd());
        // SAFETY: fcntl is called on a valid file descriptor obtained from the PTY master.
        unsafe {
            let flags = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
            if flags < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            if libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        command
            .stdin(Stdio::from(pty.slave().try_clone()?.into_owned_fd()))
            .stdout(Stdio::from(pty.slave().try_clone()?.into_owned_fd()))
            .stderr(Stdio::from(pty.slave().try_clone()?.into_owned_fd()));
        drop(pty);

        Ok(Self {
            master,
            child: command.spawn()?,
        })
    }

    fn master_mut(&mut self) -> &mut File {
        &mut self.master
    }

    fn assert_running(&mut self, label: &str) -> Result<(), Box<dyn Error>> {
        if let Some(status) = self.child.try_wait()? {
            return Err(format!("{label} attach client exited early with status {status}").into());
        }
        Ok(())
    }
}

impl Drop for PtyAttachedClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn pty_tmux_compat_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn spawn_rmux_attached_client(
    harness: &TmuxCompatHarness,
    session_name: &str,
) -> Result<PtyAttachedClient, Box<dyn Error>> {
    spawn_rmux_attached_client_with(harness, session_name, &[], &[])
}

fn spawn_rmux_attached_client_with(
    harness: &TmuxCompatHarness,
    session_name: &str,
    top_level_args: &[&str],
    environment: &[(&str, &str)],
) -> Result<PtyAttachedClient, Box<dyn Error>> {
    let home = harness.tmpdir().join("home");
    let xdg = harness.tmpdir().join("xdg");
    fs::create_dir_all(&home)?;
    fs::create_dir_all(&xdg)?;

    let mut command = Command::new(env!("CARGO_BIN_EXE_rmux"));
    command
        .env("TMPDIR", harness.tmpdir())
        .env("RMUX_TMPDIR", harness.tmpdir())
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &xdg)
        .env("TERM", "xterm-256color")
        .env_remove("RMUX");
    for (name, value) in environment {
        command.env(name, value);
    }
    command
        .args(top_level_args)
        .args(["attach-session", "-r", "-t", session_name]);
    PtyAttachedClient::spawn(command)
}

fn spawn_tmux_attached_client(
    harness: &TmuxCompatHarness,
    tmux_binary: &Path,
    session_name: &str,
) -> Result<PtyAttachedClient, Box<dyn Error>> {
    spawn_tmux_attached_client_with(harness, tmux_binary, session_name, &[], &[])
}

fn spawn_tmux_attached_client_with(
    harness: &TmuxCompatHarness,
    tmux_binary: &Path,
    session_name: &str,
    top_level_args: &[&str],
    environment: &[(&str, &str)],
) -> Result<PtyAttachedClient, Box<dyn Error>> {
    let mut command = Command::new(tmux_binary);
    command
        .env("TMPDIR", harness.tmpdir())
        .env("TMUX_TMPDIR", harness.tmpdir())
        .env("TERM", "xterm-256color")
        .env_remove("TMUX");
    for (name, value) in environment {
        command.env(name, value);
    }
    command
        .args(top_level_args)
        .arg("-S")
        .arg(harness.tmux_socket_path())
        .args(["attach-session", "-r", "-t", session_name]);
    PtyAttachedClient::spawn(command)
}

fn spawn_rmux_attached_input_client(
    harness: &TmuxCompatHarness,
    session_name: &str,
) -> Result<PtyAttachedClient, Box<dyn Error>> {
    let home = harness.tmpdir().join("home");
    let xdg = harness.tmpdir().join("xdg");
    fs::create_dir_all(&home)?;
    fs::create_dir_all(&xdg)?;

    let mut command = Command::new(env!("CARGO_BIN_EXE_rmux"));
    command
        .env("TMPDIR", harness.tmpdir())
        .env("RMUX_TMPDIR", harness.tmpdir())
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &xdg)
        .env("TERM", "xterm-256color")
        .env("LC_ALL", "C.UTF-8")
        .env("LC_CTYPE", "C.UTF-8")
        .env_remove("RMUX")
        .args(["attach-session", "-t", session_name]);
    PtyAttachedClient::spawn(command)
}

fn spawn_tmux_attached_input_client(
    harness: &TmuxCompatHarness,
    tmux_binary: &Path,
    session_name: &str,
) -> Result<PtyAttachedClient, Box<dyn Error>> {
    let mut command = Command::new(tmux_binary);
    command
        .env("TMPDIR", harness.tmpdir())
        .env("TMUX_TMPDIR", harness.tmpdir())
        .env("TERM", "xterm-256color")
        .env("LC_ALL", "C.UTF-8")
        .env("LC_CTYPE", "C.UTF-8")
        .env_remove("TMUX")
        .arg("-S")
        .arg(harness.tmux_socket_path())
        .args(["attach-session", "-t", session_name]);
    PtyAttachedClient::spawn(command)
}

struct ClusterBDeadline {
    deadline: Instant,
}

impl ClusterBDeadline {
    fn new() -> Self {
        Self {
            deadline: Instant::now() + Duration::from_secs(30),
        }
    }

    fn remaining(&self) -> Result<Duration, Box<dyn Error>> {
        self.deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| "Cluster B reproducer exceeded its 30 second bound".into())
    }

    fn check(&self) -> Result<(), Box<dyn Error>> {
        let _ = self.remaining()?;
        Ok(())
    }
}

fn cluster_b_config() -> TmuxCompatRunConfig {
    tmux_compat_config()
        .with_timeout(Duration::from_secs(5))
        .with_env("LC_ALL", "C.UTF-8")
        .with_env("LC_CTYPE", "C.UTF-8")
}

fn cluster_b_new_session(
    harness: &TmuxCompatHarness,
    tmux_binary: &Path,
    config: TmuxCompatRunConfig,
    deadline: &ClusterBDeadline,
) -> Result<(), Box<dyn Error>> {
    deadline.check()?;
    let create = harness.run_pair_with(
        tmux_binary,
        &[
            "new-session",
            "-d",
            "-s",
            "alpha",
            "-x",
            "80",
            "-y",
            "24",
            "-n",
            "bash",
        ],
        config.clone(),
    )?;
    assert_quiet_success(&create);
    let populate = harness.run_pair_with(
        tmux_binary,
        &[
            "send-keys",
            "-t",
            "alpha:0.0",
            "for i in $(seq 1 30); do printf 'P0-LINE-%02d\\n' \"$i\"; done",
            "Enter",
        ],
        config.clone(),
    )?;
    assert_quiet_success(&populate);
    let _ = wait_for_cluster_b_pair(
        harness,
        tmux_binary,
        &["capture-pane", "-p", "-S", "-", "-t", "alpha:0.0"],
        config,
        deadline,
        |run| {
            run.rmux.stdout_string().contains("P0-LINE-12")
                && run.tmux.stdout_string().contains("P0-LINE-12")
        },
    )?;
    Ok(())
}

fn wait_for_cluster_b_clients(
    harness: &TmuxCompatHarness,
    tmux_binary: &Path,
    config: TmuxCompatRunConfig,
    deadline: &ClusterBDeadline,
) -> Result<(), Box<dyn Error>> {
    let _ = wait_for_cluster_b_pair(
        harness,
        tmux_binary,
        &["list-clients", "-F", "#{session_name}"],
        config,
        deadline,
        |run| {
            run.rmux.stdout_string().contains("alpha") && run.tmux.stdout_string().contains("alpha")
        },
    )?;
    Ok(())
}

fn cluster_b_capture_pair(
    harness: &TmuxCompatHarness,
    tmux_binary: &Path,
    target: &str,
    config: TmuxCompatRunConfig,
    deadline: &ClusterBDeadline,
) -> Result<TmuxCompatRun, Box<dyn Error>> {
    deadline.check()?;
    harness.run_pair_with(
        tmux_binary,
        &["capture-pane", "-p", "-S", "-", "-t", target],
        config,
    )
}

fn wait_for_cluster_b_pair<F>(
    harness: &TmuxCompatHarness,
    tmux_binary: &Path,
    argv: &[&str],
    config: TmuxCompatRunConfig,
    deadline: &ClusterBDeadline,
    ready: F,
) -> Result<TmuxCompatRun, Box<dyn Error>>
where
    F: Fn(&TmuxCompatRun) -> bool,
{
    let mut last_detail = String::new();
    loop {
        if deadline.remaining().is_err() {
            return Err(format!(
                "Cluster B reproducer exceeded its 30 second bound: {last_detail}"
            )
            .into());
        }
        let run = harness.run_pair_with(tmux_binary, argv, config.clone())?;
        if ready(&run) {
            return Ok(run);
        }
        last_detail = format!(
            "argv={argv:?} tmux={:?}/{:?} rmux={:?}/{:?}",
            run.tmux.stdout_string(),
            run.tmux.stderr_string(),
            run.rmux.stdout_string(),
            run.rmux.stderr_string()
        );
        std::thread::sleep(Duration::from_millis(50).min(deadline.remaining()?));
    }
}

fn write_attached_keys(
    client: &mut PtyAttachedClient,
    bytes: &[u8],
    deadline: &ClusterBDeadline,
) -> Result<(), Box<dyn Error>> {
    deadline.check()?;
    client.master_mut().write_all(bytes)?;
    std::thread::sleep(Duration::from_millis(75).min(deadline.remaining()?));
    Ok(())
}

fn shutdown_cluster_b_rmux(harness: &TmuxCompatHarness) -> Result<(), Box<dyn Error>> {
    common::shutdown_rmux_server(harness.rmux_socket_path())?;
    std::thread::sleep(Duration::from_millis(500));
    Ok(())
}

fn wait_for_pair_run<F>(
    harness: &TmuxCompatHarness,
    tmux_binary: &Path,
    argv: &[&str],
    config: TmuxCompatRunConfig,
    timeout: Duration,
    ready: F,
) -> Result<TmuxCompatRun, Box<dyn Error>>
where
    F: Fn(&TmuxCompatRun) -> bool,
{
    let deadline = Instant::now() + timeout;

    loop {
        let run = harness.run_pair_with(tmux_binary, argv, config.clone())?;
        if ready(&run) {
            return Ok(run);
        }
        let detail = format!(
            "tmux stdout={:?} stderr={:?} rmux stdout={:?} stderr={:?}",
            run.tmux.stdout_string(),
            run.tmux.stderr_string(),
            run.rmux.stdout_string(),
            run.rmux.stderr_string()
        );

        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for compatibility command readiness: {}",
                detail
            )
            .into());
        }

        std::thread::sleep(Duration::from_millis(50));
    }
}

fn extract_control_frame_payload_lines(output: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut in_frame = false;

    for line in output.lines() {
        if line.starts_with("%begin ") {
            in_frame = true;
            continue;
        }
        if line.starts_with("%end ") || line.starts_with("%error ") {
            in_frame = false;
            continue;
        }
        if in_frame && !line.is_empty() {
            lines.push(line.to_owned());
        }
    }

    lines
}

fn nonempty_capture_lines(output: &str) -> Vec<String> {
    output
        .lines()
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn normalize_pts_paths(line: &str) -> String {
    let mut normalized = String::new();
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        normalized.push(ch);
        if normalized.ends_with("/dev/pts/") {
            while chars.peek().is_some_and(|next| next.is_ascii_digit()) {
                let _ = chars.next();
            }
            normalized.push('N');
        }
    }
    normalized
}

fn drain_pty(client: &mut PtyAttachedClient) -> Result<Vec<u8>, Box<dyn Error>> {
    use std::io::ErrorKind;

    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        match std::io::Read::read(client.master_mut(), &mut buffer) {
            Ok(0) => break,
            Ok(read) => bytes.extend_from_slice(&buffer[..read]),
            Err(error) if error.kind() == ErrorKind::WouldBlock => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(bytes)
}

fn render_cells(bytes: &[u8], cols: usize, rows: usize) -> Vec<String> {
    let mut screen = vec![vec![' '; cols]; rows];
    let mut row = 0usize;
    let mut col = 0usize;
    let chars = String::from_utf8_lossy(bytes).chars().collect::<Vec<_>>();
    let mut index = 0usize;

    while index < chars.len() {
        match chars[index] {
            '\u{1b}' => {
                index += 1;
                if index >= chars.len() {
                    break;
                }
                match chars[index] {
                    '[' => {
                        index += 1;
                        let mut params = String::new();
                        while index < chars.len()
                            && !chars[index].is_ascii_alphabetic()
                            && chars[index] != 'X'
                            && chars[index] != 'H'
                            && chars[index] != 'K'
                            && chars[index] != 'm'
                        {
                            params.push(chars[index]);
                            index += 1;
                        }
                        if index >= chars.len() {
                            break;
                        }
                        match chars[index] {
                            'H' => {
                                let mut parts = params.split(';');
                                row = parts
                                    .next()
                                    .and_then(|value| value.parse::<usize>().ok())
                                    .unwrap_or(1)
                                    .saturating_sub(1)
                                    .min(rows.saturating_sub(1));
                                col = parts
                                    .next()
                                    .and_then(|value| value.parse::<usize>().ok())
                                    .unwrap_or(1)
                                    .saturating_sub(1)
                                    .min(cols.saturating_sub(1));
                            }
                            'K' => {
                                for cell in screen[row].iter_mut().skip(col) {
                                    *cell = ' ';
                                }
                            }
                            'X' => {
                                let count = params.parse::<usize>().unwrap_or(1);
                                for cell in screen[row]
                                    .iter_mut()
                                    .skip(col)
                                    .take(count.min(cols.saturating_sub(col)))
                                {
                                    *cell = ' ';
                                }
                            }
                            'm' => {}
                            _ => {}
                        }
                    }
                    '(' | ')' => {
                        index += 1;
                    }
                    _ => {}
                }
            }
            '\r' => col = 0,
            '\n' => row = row.saturating_add(1).min(rows.saturating_sub(1)),
            ch => {
                if row < rows && col < cols {
                    screen[row][col] = ch;
                }
                col = col.saturating_add(1);
            }
        }
        index += 1;
    }

    screen
        .into_iter()
        .map(|line| line.into_iter().collect::<String>())
        .collect()
}

fn display_panes_overlay_visible(rendered: &str) -> bool {
    rendered.contains("39x23")
        || rendered.contains("40x23")
        || rendered.contains("39x24")
        || rendered.contains("40x24")
}

fn render_transcript(bytes: &[u8], cols: u16, rows: u16) -> String {
    let mut screen = Screen::new(ScreenTerminalSize { cols, rows }, 0);
    let mut parser = InputParser::new();
    parser.parse(bytes, &mut screen);
    String::from_utf8(screen.capture_transcript(Default::default(), Default::default()))
        .expect("captured transcript must be utf-8")
}

fn shell_quote(path: &str) -> String {
    format!("'{}'", path.replace('\'', "'\\''"))
}

#[derive(Debug)]
struct ControlModeOutput {
    status_code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run_control_mode_client(
    mut command: Command,
    commands: &str,
) -> Result<ControlModeOutput, Box<dyn Error>> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .as_mut()
        .expect("control stdin")
        .write_all(commands.as_bytes())?;

    let output = child.wait_with_output()?;
    Ok(ControlModeOutput {
        status_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

fn run_rmux_control_mode(
    harness: &TmuxCompatHarness,
    commands: &str,
) -> Result<ControlModeOutput, Box<dyn Error>> {
    run_rmux_control_mode_with(harness, commands, &[], &[])
}

fn run_rmux_control_mode_with(
    harness: &TmuxCompatHarness,
    commands: &str,
    top_level_args: &[&str],
    environment: &[(&str, &str)],
) -> Result<ControlModeOutput, Box<dyn Error>> {
    let home = harness.tmpdir().join("home");
    let xdg = harness.tmpdir().join("xdg");
    fs::create_dir_all(&home)?;
    fs::create_dir_all(&xdg)?;

    let mut command = Command::new(env!("CARGO_BIN_EXE_rmux"));
    command
        .env("TMPDIR", harness.tmpdir())
        .env("RMUX_TMPDIR", harness.tmpdir())
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &xdg)
        .env("TERM", "xterm-256color")
        .env_remove("RMUX");
    for (name, value) in environment {
        command.env(name, value);
    }
    command.args(top_level_args).arg("-C");
    run_control_mode_client(command, commands)
}

fn run_tmux_control_mode(
    harness: &TmuxCompatHarness,
    tmux_binary: &Path,
    commands: &str,
) -> Result<ControlModeOutput, Box<dyn Error>> {
    run_tmux_control_mode_with(harness, tmux_binary, commands, &[], &[])
}

fn run_tmux_control_mode_with(
    harness: &TmuxCompatHarness,
    tmux_binary: &Path,
    commands: &str,
    top_level_args: &[&str],
    environment: &[(&str, &str)],
) -> Result<ControlModeOutput, Box<dyn Error>> {
    let mut command = Command::new(tmux_binary);
    command
        .env("TMPDIR", harness.tmpdir())
        .env("TMUX_TMPDIR", harness.tmpdir())
        .env("TERM", "xterm-256color")
        .env_remove("TMUX");
    for (name, value) in environment {
        command.env(name, value);
    }
    command
        .args(top_level_args)
        .arg("-C")
        .arg("-S")
        .arg(harness.tmux_socket_path());
    run_control_mode_client(command, commands)
}

#[test]
fn tmux_compat_list_clients_attached_readonly_ignore_size_flags_when_frozen_tmux_is_available(
) -> Result<(), Box<dyn Error>> {
    let harness = TmuxCompatHarness::new("tmux-compat-list-clients-attached-flags")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let _guard = pty_tmux_compat_lock()
        .lock()
        .expect("pty compatibility lock");
    let (config, expected_overrides) = config_with_clean_homes(&harness)?;

    let create = harness.run_pair_with(
        &tmux_binary,
        &["new-session", "-d", "-s", "alpha"],
        config.clone(),
    )?;
    assert_quiet_success(&create);

    let mut rmux_attach = spawn_rmux_attached_client(&harness, "alpha")?;
    let mut tmux_attach = spawn_tmux_attached_client(&harness, &tmux_binary, "alpha")?;

    let list_clients = wait_for_pair_run(
        &harness,
        &tmux_binary,
        &["list-clients", "-F", "#{client_flags}"],
        config,
        Duration::from_secs(5),
        |run| {
            run.tmux.status_code == Some(0)
                && run.rmux.status_code == Some(0)
                && !run.tmux.stdout.is_empty()
                && !run.rmux.stdout.is_empty()
        },
    )?;
    rmux_attach.assert_running("rmux")?;
    tmux_attach.assert_running("tmux")?;
    assert_run_metadata(
        &list_clients,
        &harness,
        &tmux_binary,
        &["list-clients", "-F", "#{client_flags}"],
        &expected_overrides,
    );
    assert_exact_tmux_compat(&list_clients);
    assert_eq!(
        list_clients.tmux.stdout_string(),
        "attached,focused,ignore-size,read-only,UTF-8\n"
    );

    Ok(())
}

#[test]
fn tmux_compat_detached_new_session_ignores_populated_tmux_when_frozen_tmux_is_available(
) -> Result<(), Box<dyn Error>> {
    let harness = TmuxCompatHarness::new("tmux-compat-detached-new-session-tmux")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let (config, mut expected_overrides) = config_with_clean_homes(&harness)?;
    let tmux_value = format!("{},1,0", harness.rmux_socket_path().display());
    let config = config.with_tmux(tmux_value.clone());
    for (name, value) in &mut expected_overrides {
        if name == "TMUX" {
            *value = Some(OsString::from(&tmux_value));
        }
    }
    let argv = ["new-session", "-d", "-s", "alpha"];
    let create = harness.run_pair_with(&tmux_binary, &argv, config)?;

    assert_run_metadata(&create, &harness, &tmux_binary, &argv, &expected_overrides);
    assert_quiet_success(&create);

    Ok(())
}

#[test]
fn tmux_compat_attached_client_utf8_flags_follow_ascii_locale_without_top_level_u_when_frozen_tmux_is_available(
) -> Result<(), Box<dyn Error>> {
    let harness = TmuxCompatHarness::new("tmux-compat-client-utf8-ascii-attach")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let _guard = pty_tmux_compat_lock()
        .lock()
        .expect("pty compatibility lock");
    let (config, _) = config_with_clean_homes(&harness)?;
    let create =
        harness.run_pair_with(&tmux_binary, &["new-session", "-d", "-s", "alpha"], config)?;
    assert_quiet_success(&create);

    let client_environment = [("TERM", "vt100"), ("LC_ALL", "C"), ("LANG", "C")];
    let mut rmux_attach =
        spawn_rmux_attached_client_with(&harness, "alpha", &[], &client_environment)?;
    let mut tmux_attach =
        spawn_tmux_attached_client_with(&harness, &tmux_binary, "alpha", &[], &client_environment)?;

    let list_clients = wait_for_pair_run(
        &harness,
        &tmux_binary,
        &["list-clients", "-F", "#{client_utf8}|#{client_flags}"],
        tmux_compat_config(),
        Duration::from_secs(5),
        |run| {
            run.tmux.status_code == Some(0)
                && run.rmux.status_code == Some(0)
                && !run.tmux.stdout.is_empty()
                && !run.rmux.stdout.is_empty()
        },
    )?;
    rmux_attach.assert_running("rmux")?;
    tmux_attach.assert_running("tmux")?;
    assert_exact_tmux_compat(&list_clients);
    assert_eq!(
        list_clients.tmux.stdout_string(),
        "0|attached,focused,ignore-size,read-only\n"
    );

    Ok(())
}

#[test]
fn tmux_compat_choose_client_multi_attached_overlay_rows_when_frozen_tmux_is_available(
) -> Result<(), Box<dyn Error>> {
    let harness = TmuxCompatHarness::new("tmux-compat-choose-client-multi-overlay")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let _guard = pty_tmux_compat_lock()
        .lock()
        .expect("pty compatibility lock");
    let config = tmux_compat_config()
        .with_env("LC_ALL", "C.UTF-8")
        .with_env("LC_CTYPE", "C.UTF-8");

    let create = harness.run_pair_with(
        &tmux_binary,
        &["new-session", "-d", "-s", "alpha", "-x", "80", "-y", "24"],
        config.clone(),
    )?;
    assert_quiet_success(&create);

    let mut rmux_first = spawn_rmux_attached_client(&harness, "alpha")?;
    let mut rmux_second = spawn_rmux_attached_client(&harness, "alpha")?;
    let mut tmux_first = spawn_tmux_attached_client(&harness, &tmux_binary, "alpha")?;
    let mut tmux_second = spawn_tmux_attached_client(&harness, &tmux_binary, "alpha")?;

    let ready = Instant::now() + Duration::from_secs(5);
    while Instant::now() < ready {
        let run = harness.run_pair_with(
            &tmux_binary,
            &["list-clients", "-F", "#{session_name}"],
            config.clone(),
        )?;
        if run.rmux.stdout_string().lines().count() >= 2
            && run.tmux.stdout_string().lines().count() >= 2
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    rmux_first.assert_running("rmux")?;
    rmux_second.assert_running("rmux")?;
    tmux_first.assert_running("tmux")?;
    tmux_second.assert_running("tmux")?;
    let _ = drain_pty(&mut rmux_first)?;
    let _ = drain_pty(&mut tmux_first)?;

    let choose = harness.run_pair_with(&tmux_binary, &["choose-client"], config)?;
    assert_quiet_success(&choose);
    std::thread::sleep(Duration::from_millis(250));

    let rmux_cells = render_cells(&drain_pty(&mut rmux_first)?, 80, 24)
        .into_iter()
        .map(|line| normalize_pts_paths(line.trim_end()))
        .collect::<Vec<_>>();
    let tmux_cells = render_cells(&drain_pty(&mut tmux_first)?, 80, 24)
        .into_iter()
        .map(|line| normalize_pts_paths(line.trim_end()))
        .collect::<Vec<_>>();

    for row in [0usize, 1, 11, 22] {
        if row == 11 {
            assert_eq!(
                collapse_repeated_horizontal_borders(&rmux_cells[row]),
                collapse_repeated_horizontal_borders(&tmux_cells[row]),
                "row {row} mismatch"
            );
        } else {
            assert_eq!(rmux_cells[row], tmux_cells[row], "row {row} mismatch");
        }
    }

    Ok(())
}

#[test]
fn tmux_compat_control_mode_guard_tuple_and_exit_framing_when_frozen_tmux_is_available(
) -> Result<(), Box<dyn Error>> {
    // Cluster H compatibility scenario: check the tmux-observed `%begin`/`%end`/
    // `%error`/`%exit` tuple across the two deterministic control-mode exit
    // triggers (immediate EOF and command-followed-by-EOF). tmux terminates
    // both transcripts with a bare `%exit\n`. rmux 0.1.0 silently closes the
    // EOF-only stream and omits the trailing `%exit\n` after a command, so
    // both assertions below fail on the 0.1.0 release HEAD
    // 0b03537875071738f9a49b01b42b8b6d7f10e5a8 and pass only after the
    // EOF-to-`%exit` promotion in `forward_control` lands.
    let harness = TmuxCompatHarness::new("tmux-compat-control-mode-guard-exit-framing")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };

    // Both scenarios below run against a pre-created session so that the
    // rmux daemon and tmux server are live and plain `-C` has a control
    // client to attach to. This keeps the EOF-only scenario from degrading
    // into a no-daemon cold-start case that produces an empty transcript
    // on rmux (which is not the Cluster H compatibility target).
    let (config, _) = config_with_clean_homes(&harness)?;
    let create =
        harness.run_pair_with(&tmux_binary, &["new-session", "-d", "-s", "alpha"], config)?;
    assert_quiet_success(&create);

    // Scenario 1: immediate EOF. tmux-observed expected final tuple is the
    // bare `%exit\n` terminator (kind=exit, reason=None). Plain `-C` must
    // emit no `-CC` DCS wrapper, so the raw bytes end in `%exit\n` with no
    // `\u001b\\` suffix and no `\u001bP1000p` prefix.
    let eof_tmux = run_tmux_control_mode(&harness, &tmux_binary, "")?;
    let eof_rmux = run_rmux_control_mode(&harness, "")?;
    assert_eq!(eof_tmux.status_code, Some(0));
    assert_eq!(eof_rmux.status_code, Some(0));
    assert!(
        eof_tmux.stderr.is_empty(),
        "tmux stderr must be empty on EOF: {:?}",
        eof_tmux.stderr
    );
    assert!(
        eof_rmux.stderr.is_empty(),
        "rmux stderr must be empty on EOF: {:?}",
        eof_rmux.stderr
    );
    assert_eq!(
        last_control_line(&eof_tmux.stdout).as_deref(),
        Some("%exit"),
        "tmux EOF transcript must end with bare %exit: {:?}",
        eof_tmux.stdout
    );
    assert_eq!(
        last_control_line(&eof_rmux.stdout).as_deref(),
        Some("%exit"),
        "rmux EOF transcript must end with bare %exit: {:?}",
        eof_rmux.stdout
    );
    assert!(
        !eof_rmux.stdout.contains(rmux_proto::CONTROL_CONTROL_START),
        "plain -C must not emit the -CC DCS prefix: {:?}",
        eof_rmux.stdout
    );
    assert!(
        !eof_rmux.stdout.contains(rmux_proto::CONTROL_CONTROL_END),
        "plain -C must not emit the -CC DCS suffix: {:?}",
        eof_rmux.stdout
    );
    assert!(
        !eof_tmux.stdout.contains(rmux_proto::CONTROL_CONTROL_START),
        "tmux plain -C must not emit the -CC DCS prefix: {:?}",
        eof_tmux.stdout
    );

    // Scenario 2: single command + EOF. The tmux-observed tuple shape for
    // the user command is (Begin, <time>, N, 1) paired with (End, <time>,
    // N, 1), followed by a bare `%exit\n`. The `<time>` field is
    // wall-clock and is normalized away; the command number is normalized
    // within each implementation because tmux uses a long-lived global
    // counter and rmux restarts it per control session, but the paired
    // begin/end must reuse the exact same command number and the flags
    // column must be `1` on both.
    let commands = "display-message -p hello\n";
    let cmd_tmux = run_tmux_control_mode(&harness, &tmux_binary, commands)?;
    let cmd_rmux = run_rmux_control_mode(&harness, commands)?;
    assert_eq!(cmd_tmux.status_code, Some(0));
    assert_eq!(cmd_rmux.status_code, Some(0));
    assert!(cmd_tmux.stderr.is_empty());
    assert!(cmd_rmux.stderr.is_empty());
    assert_eq!(
        last_control_line(&cmd_tmux.stdout).as_deref(),
        Some("%exit"),
        "tmux command transcript must end with bare %exit: {:?}",
        cmd_tmux.stdout
    );
    assert_eq!(
        last_control_line(&cmd_rmux.stdout).as_deref(),
        Some("%exit"),
        "rmux command transcript must end with bare %exit: {:?}",
        cmd_rmux.stdout
    );

    let tmux_guards = control_guard_tuples(&cmd_tmux.stdout);
    let rmux_guards = control_guard_tuples(&cmd_rmux.stdout);
    let tmux_last_begin = tmux_guards
        .iter()
        .rev()
        .find(|guard| guard.kind == "begin")
        .expect("tmux must emit at least one %begin for the user command");
    let tmux_last_end = tmux_guards
        .iter()
        .rev()
        .find(|guard| guard.kind == "end")
        .expect("tmux must emit at least one %end for the user command");
    let rmux_last_begin = rmux_guards
        .iter()
        .rev()
        .find(|guard| guard.kind == "begin")
        .expect("rmux must emit at least one %begin for the user command");
    let rmux_last_end = rmux_guards
        .iter()
        .rev()
        .find(|guard| guard.kind == "end")
        .expect("rmux must emit at least one %end for the user command");
    assert_eq!(
        tmux_last_begin.flags, 1,
        "tmux anchors user-command %begin flags to 1"
    );
    assert_eq!(
        tmux_last_end.flags, 1,
        "tmux anchors user-command %end flags to 1"
    );
    assert_eq!(rmux_last_begin.flags, tmux_last_begin.flags);
    assert_eq!(rmux_last_end.flags, tmux_last_end.flags);
    assert_eq!(
        tmux_last_begin.command_number, tmux_last_end.command_number,
        "tmux pairs %begin and %end with the same command number"
    );
    assert_eq!(
        rmux_last_begin.command_number, rmux_last_end.command_number,
        "rmux must pair %begin and %end with the same command number"
    );

    let tmux_payload = extract_control_frame_payload_lines(&cmd_tmux.stdout);
    let rmux_payload = extract_control_frame_payload_lines(&cmd_rmux.stdout);
    assert!(
        tmux_payload.iter().any(|line| line == "hello"),
        "tmux payload must contain the display-message output: {tmux_payload:?}"
    );
    assert!(
        rmux_payload.iter().any(|line| line == "hello"),
        "rmux payload must contain the display-message output: {rmux_payload:?}"
    );

    // Every begin/end guard in the rmux transcript must advertise flags=1
    // and pair with a matching close kind that reuses the same command
    // number. This is the Cluster H invariant on the emit-side, independent
    // of whether tmux reports a different absolute command-number offset.
    assert!(
        !rmux_guards.is_empty(),
        "rmux transcript must contain at least one guard tuple: {:?}",
        cmd_rmux.stdout
    );
    for guard in &rmux_guards {
        assert_eq!(
            guard.flags, 1,
            "every rmux guard flags column must be 1: {guard:?}"
        );
    }
    let rmux_begin_numbers = rmux_guards
        .iter()
        .filter(|guard| guard.kind == "begin")
        .map(|guard| guard.command_number)
        .collect::<Vec<_>>();
    assert!(
        rmux_begin_numbers.iter().all(|number| *number >= 1),
        "rmux command numbers must be positive: {rmux_begin_numbers:?}"
    );
    assert!(
        rmux_begin_numbers.windows(2).all(|pair| pair[1] > pair[0]),
        "rmux command numbers must be strictly monotonic: {rmux_begin_numbers:?}"
    );

    // Plain `-C` must not wrap either transcript in the `-CC` DCS envelope.
    assert!(
        !cmd_rmux.stdout.contains(rmux_proto::CONTROL_CONTROL_START),
        "plain -C command transcript must not contain DCS prefix: {:?}",
        cmd_rmux.stdout
    );
    assert!(
        !cmd_rmux.stdout.contains(rmux_proto::CONTROL_CONTROL_END),
        "plain -C command transcript must not contain DCS suffix: {:?}",
        cmd_rmux.stdout
    );
    assert!(
        !cmd_tmux.stdout.contains(rmux_proto::CONTROL_CONTROL_START),
        "tmux plain -C command transcript must not contain DCS prefix: {:?}",
        cmd_tmux.stdout
    );

    // Scenario 3: command parse failure + EOF. The tmux-observed failure
    // tuple is (Begin, <time>, N, 1) followed by (Error, <time>, N, 1)
    // and then the same bare `%exit\n` terminator. The concrete diagnostic
    // text is not part of Cluster H; this assertion checks the tuple shape
    // and the flags/command-number relationship.
    let error_commands = "no-such-rmux-cluster-h-command\n";
    let error_tmux = run_tmux_control_mode(&harness, &tmux_binary, error_commands)?;
    let error_rmux = run_rmux_control_mode(&harness, error_commands)?;
    assert_eq!(error_tmux.status_code, Some(0));
    assert_eq!(error_rmux.status_code, Some(0));
    assert!(error_tmux.stderr.is_empty());
    assert!(error_rmux.stderr.is_empty());
    assert_eq!(
        last_control_line(&error_tmux.stdout).as_deref(),
        Some("%exit"),
        "tmux error transcript must end with bare %exit: {:?}",
        error_tmux.stdout
    );
    assert_eq!(
        last_control_line(&error_rmux.stdout).as_deref(),
        Some("%exit"),
        "rmux error transcript must end with bare %exit: {:?}",
        error_rmux.stdout
    );

    let tmux_error_guards = control_guard_tuples(&error_tmux.stdout);
    let rmux_error_guards = control_guard_tuples(&error_rmux.stdout);
    let tmux_error_begin = tmux_error_guards
        .iter()
        .rev()
        .find(|guard| guard.kind == "begin")
        .expect("tmux must emit %begin before the parse error");
    let tmux_error = tmux_error_guards
        .iter()
        .rev()
        .find(|guard| guard.kind == "error")
        .expect("tmux must emit %error for the parse error");
    let rmux_error_begin = rmux_error_guards
        .iter()
        .rev()
        .find(|guard| guard.kind == "begin")
        .expect("rmux must emit %begin before the parse error");
    let rmux_error = rmux_error_guards
        .iter()
        .rev()
        .find(|guard| guard.kind == "error")
        .expect("rmux must emit %error for the parse error");
    assert_eq!(tmux_error_begin.flags, 1);
    assert_eq!(tmux_error.flags, 1);
    assert_eq!(rmux_error_begin.flags, tmux_error_begin.flags);
    assert_eq!(rmux_error.flags, tmux_error.flags);
    assert_eq!(
        tmux_error_begin.command_number, tmux_error.command_number,
        "tmux pairs parse-error %begin and %error with one command number"
    );
    assert_eq!(
        rmux_error_begin.command_number, rmux_error.command_number,
        "rmux must pair parse-error %begin and %error with one command number"
    );

    Ok(())
}

#[derive(Debug, Clone)]
struct ControlGuardTuple {
    kind: String,
    command_number: u64,
    flags: u8,
}

fn control_guard_tuples(output: &str) -> Vec<ControlGuardTuple> {
    let mut guards = Vec::new();
    for line in output.lines() {
        let parsed = line
            .strip_prefix("%begin ")
            .map(|rest| ("begin", rest))
            .or_else(|| line.strip_prefix("%end ").map(|rest| ("end", rest)))
            .or_else(|| line.strip_prefix("%error ").map(|rest| ("error", rest)));
        let Some((kind, rest)) = parsed else {
            continue;
        };
        let mut parts = rest.split_whitespace();
        let _time = parts.next();
        let command_number = parts.next().and_then(|value| value.parse::<u64>().ok());
        let flags = parts.next().and_then(|value| value.parse::<u8>().ok());
        if let (Some(command_number), Some(flags)) = (command_number, flags) {
            guards.push(ControlGuardTuple {
                kind: kind.to_owned(),
                command_number,
                flags,
            });
        }
    }
    guards
}

fn last_control_line(output: &str) -> Option<String> {
    output
        .lines()
        .rfind(|line| !line.is_empty())
        .map(ToOwned::to_owned)
}

#[test]
fn tmux_compat_list_clients_control_mode_flags_when_frozen_tmux_is_available(
) -> Result<(), Box<dyn Error>> {
    let harness = TmuxCompatHarness::new("tmux-compat-list-clients-control-flags")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let (config, _) = config_with_clean_homes(&harness)?;
    let create =
        harness.run_pair_with(&tmux_binary, &["new-session", "-d", "-s", "alpha"], config)?;
    assert_quiet_success(&create);

    let commands = "attach-session -t alpha\nlist-clients -F '#{client_flags}'\n";
    let tmux_run = run_tmux_control_mode(&harness, &tmux_binary, commands)?;
    let rmux_run = run_rmux_control_mode(&harness, commands)?;
    assert_eq!(tmux_run.status_code, Some(0));
    assert_eq!(rmux_run.status_code, Some(0));
    assert!(tmux_run.stderr.is_empty());
    assert!(rmux_run.stderr.is_empty());

    let tmux_flags = extract_control_frame_payload_lines(&tmux_run.stdout);
    let rmux_flags = extract_control_frame_payload_lines(&rmux_run.stdout);
    assert_eq!(rmux_flags, tmux_flags);
    assert_eq!(
        tmux_flags,
        vec!["attached,focused,control-mode,UTF-8".to_owned()]
    );

    Ok(())
}

#[test]
fn tmux_compat_attached_client_top_level_terminal_runtime_overrides_when_frozen_tmux_is_available(
) -> Result<(), Box<dyn Error>> {
    let harness = TmuxCompatHarness::new("tmux-compat-client-runtime-top-level-attach")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let _guard = pty_tmux_compat_lock()
        .lock()
        .expect("pty compatibility lock");
    let (config, _) = config_with_clean_homes(&harness)?;
    let create =
        harness.run_pair_with(&tmux_binary, &["new-session", "-d", "-s", "alpha"], config)?;
    assert_quiet_success(&create);

    let client_environment = [("TERM", "vt100"), ("LC_ALL", "C"), ("LANG", "C")];
    let top_level_args = ["-u", "-2", "-T", "RGB"];
    let mut rmux_attach =
        spawn_rmux_attached_client_with(&harness, "alpha", &top_level_args, &client_environment)?;
    let mut tmux_attach = spawn_tmux_attached_client_with(
        &harness,
        &tmux_binary,
        "alpha",
        &top_level_args,
        &client_environment,
    )?;

    let list_clients = wait_for_pair_run(
        &harness,
        &tmux_binary,
        &[
            "list-clients",
            "-F",
            "#{client_termname}|#{client_termtype}|#{client_termfeatures}|#{client_utf8}|#{client_flags}",
        ],
        tmux_compat_config(),
        Duration::from_secs(5),
        |run| {
            run.tmux.status_code == Some(0)
                && run.rmux.status_code == Some(0)
                && !run.tmux.stdout.is_empty()
                && !run.rmux.stdout.is_empty()
        },
    )?;
    rmux_attach.assert_running("rmux")?;
    tmux_attach.assert_running("tmux")?;
    let tmux_line = list_clients.tmux.stdout_string();
    let rmux_line = list_clients.rmux.stdout_string();
    let tmux_parts = tmux_line
        .trim_end()
        .split('|')
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    let rmux_parts = rmux_line
        .trim_end()
        .split('|')
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    assert_eq!(tmux_parts.len(), 5);
    assert_eq!(rmux_parts.len(), 5);
    assert_eq!(tmux_parts[0], "vt100");
    assert_eq!(rmux_parts[0], tmux_parts[0]);
    assert_eq!(rmux_parts[1], tmux_parts[1]);
    assert_eq!(rmux_parts[3], tmux_parts[3]);
    assert_eq!(rmux_parts[4], tmux_parts[4]);
    assert_eq!(tmux_parts[3], "1");
    assert!(
        tmux_parts[2].split(',').any(|feature| feature == "256"),
        "expected tmux termfeatures to include 256, got {:?}",
        tmux_parts[2]
    );
    assert!(
        tmux_parts[2].split(',').any(|feature| feature == "RGB"),
        "expected tmux termfeatures to include RGB, got {:?}",
        tmux_parts[2]
    );
    assert!(
        rmux_parts[2].split(',').any(|feature| feature == "256"),
        "expected rmux termfeatures to include 256, got {:?}",
        rmux_parts[2]
    );
    assert!(
        rmux_parts[2].split(',').any(|feature| feature == "RGB"),
        "expected rmux termfeatures to include RGB, got {:?}",
        rmux_parts[2]
    );

    Ok(())
}

#[test]
fn tmux_compat_control_mode_top_level_terminal_runtime_overrides_when_frozen_tmux_is_available(
) -> Result<(), Box<dyn Error>> {
    let harness = TmuxCompatHarness::new("tmux-compat-client-runtime-top-level-control")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let (config, _) = config_with_clean_homes(&harness)?;
    let create =
        harness.run_pair_with(&tmux_binary, &["new-session", "-d", "-s", "alpha"], config)?;
    assert_quiet_success(&create);

    let client_environment = [("TERM", "vt100"), ("LC_ALL", "C"), ("LANG", "C")];
    let top_level_args = ["-u", "-2", "-T", "RGB"];
    let commands = "attach-session -t alpha\nlist-clients -F '#{client_termname}|#{client_termtype}|#{client_termfeatures}|#{client_utf8}|#{client_flags}'\n";
    let tmux_run = run_tmux_control_mode_with(
        &harness,
        &tmux_binary,
        commands,
        &top_level_args,
        &client_environment,
    )?;
    let rmux_run =
        run_rmux_control_mode_with(&harness, commands, &top_level_args, &client_environment)?;
    assert_eq!(tmux_run.status_code, Some(0));
    assert_eq!(rmux_run.status_code, Some(0));
    assert!(tmux_run.stderr.is_empty());
    assert!(rmux_run.stderr.is_empty());

    let tmux_lines = extract_control_frame_payload_lines(&tmux_run.stdout);
    let rmux_lines = extract_control_frame_payload_lines(&rmux_run.stdout);
    assert_eq!(rmux_lines, tmux_lines);
    assert_eq!(
        tmux_lines,
        vec!["vt100||256,RGB|1|attached,focused,control-mode,UTF-8".to_owned()]
    );

    Ok(())
}

#[test]
fn tmux_compat_new_window_control_mode_start_directory_and_shell_command_when_frozen_tmux_is_available(
) -> Result<(), Box<dyn Error>> {
    let harness = TmuxCompatHarness::new("tmux-compat-new-window-control-mode-spawn")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let config = tmux_compat_config();
    let start_directory = harness.tmpdir().join("new-window-cwd");
    fs::create_dir_all(&start_directory)?;
    let start_directory = start_directory.to_string_lossy().into_owned();

    let create = harness.run_pair_with(
        &tmux_binary,
        &["new-session", "-d", "-s", "alpha"],
        config.clone(),
    )?;
    assert_quiet_success(&create);

    let commands = format!(
        "new-window -d -t alpha -c {} -- sh -c 'pwd; printf \"ARGV0=[%s]\\n\" \"$0\"; printf \"ARGV=\"; for arg in \"$@\"; do printf \"[%s]\" \"$arg\"; done; printf \"\\n\"; printf \"shell=quoted ; value\\n\"; exec sleep 30' foo 'bar baz'\n",
        shell_quote(&start_directory)
    );
    let tmux_run = run_tmux_control_mode(&harness, &tmux_binary, &commands)?;
    let rmux_run = run_rmux_control_mode(&harness, &commands)?;
    assert_eq!(tmux_run.status_code, Some(0));
    assert_eq!(rmux_run.status_code, Some(0));
    assert!(tmux_run.stderr.is_empty());
    assert!(rmux_run.stderr.is_empty());

    let expected_lines = vec![
        start_directory.clone(),
        "ARGV0=[foo]".to_owned(),
        "ARGV=[bar baz]".to_owned(),
        "shell=quoted ; value".to_owned(),
    ];
    let capture = wait_for_pair_run(
        &harness,
        &tmux_binary,
        &["capture-pane", "-p", "-t", "alpha:1.0"],
        config,
        Duration::from_secs(5),
        |run| {
            run.tmux.status_code == Some(0)
                && run.rmux.status_code == Some(0)
                && nonempty_capture_lines(&run.tmux.stdout_string()) == expected_lines
                && nonempty_capture_lines(&run.rmux.stdout_string()) == expected_lines
        },
    )?;
    assert_exact_tmux_compat(&capture);

    Ok(())
}

#[test]
fn tmux_compat_respawn_window_control_mode_start_directory_and_shell_command_when_frozen_tmux_is_available(
) -> Result<(), Box<dyn Error>> {
    let harness = TmuxCompatHarness::new("tmux-compat-respawn-window-control-mode-spawn")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let config = tmux_compat_config();
    let start_directory = harness.tmpdir().join("respawn-window-cwd");
    fs::create_dir_all(&start_directory)?;
    let start_directory = start_directory.to_string_lossy().into_owned();

    let create = harness.run_pair_with(
        &tmux_binary,
        &["new-session", "-d", "-s", "alpha"],
        config.clone(),
    )?;
    assert_quiet_success(&create);

    let commands = format!(
        "respawn-window -k -t alpha:0 -c {} -- sh -c 'pwd; printf \"ARGV0=[%s]\\n\" \"$0\"; printf \"ARGV=\"; for arg in \"$@\"; do printf \"[%s]\" \"$arg\"; done; printf \"\\n\"; printf \"shell=quoted ; value\\n\"; exec sleep 30' foo 'bar baz'\n",
        shell_quote(&start_directory)
    );
    let tmux_run = run_tmux_control_mode(&harness, &tmux_binary, &commands)?;
    let rmux_run = run_rmux_control_mode(&harness, &commands)?;
    assert_eq!(tmux_run.status_code, Some(0));
    assert_eq!(rmux_run.status_code, Some(0));
    assert!(tmux_run.stderr.is_empty());
    assert!(rmux_run.stderr.is_empty());

    let expected_lines = vec![
        start_directory.clone(),
        "ARGV0=[foo]".to_owned(),
        "ARGV=[bar baz]".to_owned(),
        "shell=quoted ; value".to_owned(),
    ];
    let capture = wait_for_pair_run(
        &harness,
        &tmux_binary,
        &["capture-pane", "-p", "-t", "alpha:0.0"],
        config,
        Duration::from_secs(5),
        |run| {
            run.tmux.status_code == Some(0)
                && run.rmux.status_code == Some(0)
                && nonempty_capture_lines(&run.tmux.stdout_string()) == expected_lines
                && nonempty_capture_lines(&run.rmux.stdout_string()) == expected_lines
        },
    )?;
    assert_exact_tmux_compat(&capture);

    Ok(())
}

#[test]
fn tmux_compat_control_mode_window_id_targets_and_new_window_exact_slot_when_frozen_tmux_is_available(
) -> Result<(), Box<dyn Error>> {
    let harness = TmuxCompatHarness::new("tmux-compat-control-mode-window-id-targets")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let config = tmux_compat_config();

    let create = harness.run_pair_with(
        &tmux_binary,
        &["new-session", "-d", "-s", "alpha"],
        config.clone(),
    )?;
    assert_quiet_success(&create);

    let window_id_run = harness.run_pair_with(
        &tmux_binary,
        &["display-message", "-p", "-t", "alpha:0", "#{window_id}"],
        config.clone(),
    )?;
    assert_exact_tmux_compat(&window_id_run);
    let window_id = window_id_run.tmux.stdout_string().trim().to_owned();

    let new_window_commands = "new-window -d -t alpha:2 -- sleep 30\n";
    let tmux_new_window = run_tmux_control_mode(&harness, &tmux_binary, new_window_commands)?;
    let rmux_new_window = run_rmux_control_mode(&harness, new_window_commands)?;
    assert_eq!(tmux_new_window.status_code, Some(0));
    assert_eq!(rmux_new_window.status_code, Some(0));
    assert!(tmux_new_window.stderr.is_empty());
    assert!(rmux_new_window.stderr.is_empty());

    let new_window_display = wait_for_pair_run(
        &harness,
        &tmux_binary,
        &[
            "display-message",
            "-p",
            "-t",
            "alpha:2",
            "#{window_index}|#{pane_current_command}",
        ],
        config.clone(),
        Duration::from_secs(5),
        |run| {
            run.tmux.status_code == Some(0)
                && run.rmux.status_code == Some(0)
                && run.tmux.stdout == b"2|sleep\n"
                && run.rmux.stdout == b"2|sleep\n"
        },
    )?;
    assert_exact_tmux_compat(&new_window_display);

    let respawn_and_display_commands = format!("respawn-window -k -t {window_id} -- sleep 30\n");
    let tmux_respawn =
        run_tmux_control_mode(&harness, &tmux_binary, &respawn_and_display_commands)?;
    let rmux_respawn = run_rmux_control_mode(&harness, &respawn_and_display_commands)?;
    assert_eq!(tmux_respawn.status_code, Some(0));
    assert_eq!(rmux_respawn.status_code, Some(0));
    assert!(tmux_respawn.stderr.is_empty());
    assert!(rmux_respawn.stderr.is_empty());

    let expected_respawn = format!("alpha|0|{window_id}|sleep");
    let respawn_display_commands = format!(
        "display-message -p -t {window_id} '#{{session_name}}|#{{window_index}}|#{{window_id}}|#{{pane_current_command}}'\n"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let (tmux_display, rmux_display) = loop {
        let tmux_display =
            run_tmux_control_mode(&harness, &tmux_binary, &respawn_display_commands)?;
        let rmux_display = run_rmux_control_mode(&harness, &respawn_display_commands)?;
        if tmux_display.status_code == Some(0)
            && rmux_display.status_code == Some(0)
            && tmux_display.stderr.is_empty()
            && rmux_display.stderr.is_empty()
            && extract_control_frame_payload_lines(&tmux_display.stdout)
                == vec![expected_respawn.clone()]
            && extract_control_frame_payload_lines(&rmux_display.stdout)
                == vec![expected_respawn.clone()]
        {
            break (tmux_display, rmux_display);
        }

        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for respawn-window compatibility readiness: tmux stdout={:?} stderr={:?} rmux stdout={:?} stderr={:?}",
                tmux_display.stdout, tmux_display.stderr, rmux_display.stdout, rmux_display.stderr
            )
            .into());
        }

        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(
        extract_control_frame_payload_lines(&tmux_display.stdout),
        vec![expected_respawn.clone()]
    );
    assert_eq!(
        extract_control_frame_payload_lines(&rmux_display.stdout),
        vec![expected_respawn]
    );

    Ok(())
}

#[test]
fn tmux_compat_new_window_start_directory_when_frozen_tmux_is_available(
) -> Result<(), Box<dyn Error>> {
    let harness = TmuxCompatHarness::new("tmux-compat-new-window-start-directory")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let config = tmux_compat_config();
    let expected_overrides = default_overrides(harness.tmpdir());
    let start_directory = harness.tmpdir().join("new-window-cwd");
    fs::create_dir_all(&start_directory)?;
    let start_directory = start_directory.to_string_lossy().into_owned();

    let create = harness.run_pair_with(
        &tmux_binary,
        &["new-session", "-d", "-s", "alpha"],
        config.clone(),
    )?;
    assert_quiet_success(&create);
    assert_run_metadata(
        &create,
        &harness,
        &tmux_binary,
        &["new-session", "-d", "-s", "alpha"],
        &expected_overrides,
    );

    let action_args = [
        "new-window",
        "-d",
        "-t",
        "alpha",
        "-c",
        start_directory.as_str(),
        "sleep",
        "30",
    ];
    let new_window = harness.run_pair_with(&tmux_binary, &action_args, config.clone())?;
    assert_exact_tmux_compat(&new_window);
    assert_run_metadata(
        &new_window,
        &harness,
        &tmux_binary,
        &action_args,
        &expected_overrides,
    );

    let expected_display = format!("{start_directory}|sleep\n");
    let display_args = [
        "display-message",
        "-p",
        "-t",
        "alpha:1",
        "#{pane_current_path}|#{pane_current_command}",
    ];
    let display = wait_for_pair_run(
        &harness,
        &tmux_binary,
        &display_args,
        config,
        Duration::from_secs(5),
        |run| {
            run.tmux.status_code == Some(0)
                && run.rmux.status_code == Some(0)
                && run.tmux.stdout == run.rmux.stdout
                && run.tmux.stdout_string() == expected_display
        },
    )?;
    assert_exact_tmux_compat(&display);
    assert_run_metadata(
        &display,
        &harness,
        &tmux_binary,
        &display_args,
        &expected_overrides,
    );

    Ok(())
}

#[test]
fn tmux_compat_new_window_shell_command_when_frozen_tmux_is_available() -> Result<(), Box<dyn Error>>
{
    let harness = TmuxCompatHarness::new("tmux-compat-new-window-shell-command")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let config = tmux_compat_config();
    let expected_overrides = default_overrides(harness.tmpdir());
    let shell_command = "printf hi; exec sleep 30";

    let create = harness.run_pair_with(
        &tmux_binary,
        &["new-session", "-d", "-s", "alpha"],
        config.clone(),
    )?;
    assert_quiet_success(&create);
    assert_run_metadata(
        &create,
        &harness,
        &tmux_binary,
        &["new-session", "-d", "-s", "alpha"],
        &expected_overrides,
    );

    let action_args = ["new-window", "-d", "-t", "alpha", "--", shell_command];
    let new_window = harness.run_pair_with(&tmux_binary, &action_args, config.clone())?;
    assert_exact_tmux_compat(&new_window);
    assert_run_metadata(
        &new_window,
        &harness,
        &tmux_binary,
        &action_args,
        &expected_overrides,
    );

    let display_args = [
        "display-message",
        "-p",
        "-t",
        "alpha:1",
        "#{pane_current_command}",
    ];
    let display = wait_for_pair_run(
        &harness,
        &tmux_binary,
        &display_args,
        config,
        Duration::from_secs(5),
        |run| {
            run.tmux.status_code == Some(0)
                && run.rmux.status_code == Some(0)
                && run.tmux.stdout == b"sleep\n"
                && run.rmux.stdout == b"sleep\n"
        },
    )?;
    assert_exact_tmux_compat(&display);
    assert_run_metadata(
        &display,
        &harness,
        &tmux_binary,
        &display_args,
        &expected_overrides,
    );

    Ok(())
}

#[test]
fn tmux_compat_respawn_window_start_directory_when_frozen_tmux_is_available(
) -> Result<(), Box<dyn Error>> {
    let harness = TmuxCompatHarness::new("tmux-compat-respawn-window-start-directory")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let config = tmux_compat_config();
    let expected_overrides = default_overrides(harness.tmpdir());
    let start_directory = harness.tmpdir().join("respawn-window-cwd");
    fs::create_dir_all(&start_directory)?;
    let start_directory = start_directory.to_string_lossy().into_owned();

    let create = harness.run_pair_with(
        &tmux_binary,
        &["new-session", "-d", "-s", "alpha"],
        config.clone(),
    )?;
    assert_quiet_success(&create);
    assert_run_metadata(
        &create,
        &harness,
        &tmux_binary,
        &["new-session", "-d", "-s", "alpha"],
        &expected_overrides,
    );

    let action_args = [
        "respawn-window",
        "-k",
        "-t",
        "alpha:0",
        "-c",
        start_directory.as_str(),
        "--",
        "sleep",
        "30",
    ];
    let respawn = harness.run_pair_with(&tmux_binary, &action_args, config.clone())?;
    assert_exact_tmux_compat(&respawn);
    assert_run_metadata(
        &respawn,
        &harness,
        &tmux_binary,
        &action_args,
        &expected_overrides,
    );

    let expected_display = format!("{start_directory}|sleep\n");
    let display_args = [
        "display-message",
        "-p",
        "-t",
        "alpha:0",
        "#{pane_current_path}|#{pane_current_command}",
    ];
    let display = wait_for_pair_run(
        &harness,
        &tmux_binary,
        &display_args,
        config,
        Duration::from_secs(5),
        |run| {
            run.tmux.status_code == Some(0)
                && run.rmux.status_code == Some(0)
                && run.tmux.stdout == run.rmux.stdout
                && run.tmux.stdout_string() == expected_display
        },
    )?;
    assert_exact_tmux_compat(&display);
    assert_run_metadata(
        &display,
        &harness,
        &tmux_binary,
        &display_args,
        &expected_overrides,
    );

    Ok(())
}

#[test]
fn tmux_compat_respawn_window_shell_command_when_frozen_tmux_is_available(
) -> Result<(), Box<dyn Error>> {
    let harness = TmuxCompatHarness::new("tmux-compat-respawn-window-shell-command")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let config = tmux_compat_config();
    let expected_overrides = default_overrides(harness.tmpdir());
    let shell_command = "printf hi; exec sleep 30";

    let create = harness.run_pair_with(
        &tmux_binary,
        &["new-session", "-d", "-s", "alpha"],
        config.clone(),
    )?;
    assert_quiet_success(&create);
    assert_run_metadata(
        &create,
        &harness,
        &tmux_binary,
        &["new-session", "-d", "-s", "alpha"],
        &expected_overrides,
    );

    let action_args = ["respawn-window", "-k", "-t", "alpha:0", "--", shell_command];
    let respawn = harness.run_pair_with(&tmux_binary, &action_args, config.clone())?;
    assert_exact_tmux_compat(&respawn);
    assert_run_metadata(
        &respawn,
        &harness,
        &tmux_binary,
        &action_args,
        &expected_overrides,
    );

    let display_args = [
        "display-message",
        "-p",
        "-t",
        "alpha:0",
        "#{pane_current_command}",
    ];
    let display = wait_for_pair_run(
        &harness,
        &tmux_binary,
        &display_args,
        config,
        Duration::from_secs(5),
        |run| {
            run.tmux.status_code == Some(0)
                && run.rmux.status_code == Some(0)
                && run.tmux.stdout == b"sleep\n"
                && run.rmux.stdout == b"sleep\n"
        },
    )?;
    assert_exact_tmux_compat(&display);
    assert_run_metadata(
        &display,
        &harness,
        &tmux_binary,
        &display_args,
        &expected_overrides,
    );

    Ok(())
}

#[test]
fn tmux_compat_list_commands_output_when_frozen_tmux_is_available() -> Result<(), Box<dyn Error>> {
    let harness = TmuxCompatHarness::new("tmux-compat-list-commands")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let config = tmux_compat_config();
    let expected_overrides = default_overrides(harness.tmpdir());

    let run = harness.run_pair_with(&tmux_binary, &["list-commands"], config)?;
    assert_run_metadata(
        &run,
        &harness,
        &tmux_binary,
        &["list-commands"],
        &expected_overrides,
    );
    assert_eq!(run.tmux.status_code, Some(0));
    assert_eq!(run.rmux.status_code, Some(0));
    assert!(!run.tmux.timed_out);
    assert!(!run.rmux.timed_out);
    assert!(run.tmux.stderr_string().is_empty());
    assert!(run.rmux.stderr_string().is_empty());

    // Both must list the same commands; rmux uses its own aliases so we compare
    // the sorted set of primary command names (first word on each line).
    let tmux_commands = sorted_first_words(&run.tmux.stdout_string());
    let rmux_commands = sorted_first_words(&run.rmux.stdout_string());
    assert!(
        !tmux_commands.is_empty(),
        "tmux list-commands produced no output"
    );
    assert!(
        !rmux_commands.is_empty(),
        "rmux list-commands produced no output"
    );

    // rmux may support a subset; verify every rmux command also appears in tmux
    for cmd in &rmux_commands {
        assert!(
            tmux_commands.contains(cmd),
            "rmux lists command {cmd:?} which is absent from tmux"
        );
    }

    Ok(())
}

#[test]
fn tmux_compat_unknown_command_error_exit_when_frozen_tmux_is_available(
) -> Result<(), Box<dyn Error>> {
    let harness = TmuxCompatHarness::new("tmux-compat-unknown-cmd")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let config = tmux_compat_config();
    let expected_overrides = default_overrides(harness.tmpdir());

    let run = harness.run_pair_with(&tmux_binary, &["nonexistent-command-xyz"], config)?;
    assert_run_metadata(
        &run,
        &harness,
        &tmux_binary,
        &["nonexistent-command-xyz"],
        &expected_overrides,
    );

    // Both should exit with non-zero status
    assert!(
        run.tmux.status_code != Some(0),
        "tmux should reject unknown command"
    );
    assert!(
        run.rmux.status_code != Some(0),
        "rmux should reject unknown command"
    );
    assert!(!run.tmux.timed_out);
    assert!(!run.rmux.timed_out);

    // Both should produce stderr
    assert!(
        !run.tmux.stderr_string().is_empty(),
        "tmux should print an error for unknown command"
    );
    assert!(
        !run.rmux.stderr_string().is_empty(),
        "rmux should print an error for unknown command"
    );

    Ok(())
}

#[test]
fn tmux_compat_help_usage_line_when_frozen_tmux_is_available() -> Result<(), Box<dyn Error>> {
    let harness = TmuxCompatHarness::new("tmux-compat-help-usage")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let config = tmux_compat_config();

    // -h is a client flag that prints usage
    let rmux_run = harness.run_pair_with(&tmux_binary, &["-h"], config)?;
    assert!(!rmux_run.tmux.timed_out);
    assert!(!rmux_run.rmux.timed_out);

    // tmux -h prints usage to stdout and exits 1; rmux matches the exit behavior
    // but the usage text references different binary names -- verify structural compatibility
    let tmux_usage = rmux_run.tmux.stdout_string();
    let rmux_usage = rmux_run.rmux.stdout_string();

    assert!(
        tmux_usage.contains("usage:") || tmux_usage.contains("Usage"),
        "tmux -h should print usage, got: {tmux_usage:?}"
    );
    assert!(
        rmux_usage.contains("usage:") || rmux_usage.contains("Usage"),
        "rmux -h should print usage, got: {rmux_usage:?}"
    );

    // Both should include the common flags
    for flag in &["-L", "-S", "-f"] {
        assert!(
            rmux_usage.contains(flag),
            "rmux usage missing flag {flag}: {rmux_usage:?}"
        );
    }

    Ok(())
}

#[test]
fn tmux_compat_user_option_set_and_show_when_frozen_tmux_is_available() -> Result<(), Box<dyn Error>>
{
    let harness = TmuxCompatHarness::new("tmux-compat-user-option")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let config = tmux_compat_config();
    let expected_overrides = default_overrides(harness.tmpdir());

    let create = harness.run_pair_with(
        &tmux_binary,
        &["new-session", "-d", "-s", "alpha"],
        config.clone(),
    )?;
    assert_quiet_success(&create);

    // Set a user option (prefixed with @)
    let set_user = harness.run_pair_with(
        &tmux_binary,
        &["set", "-g", "@my-user-opt", "hello-world"],
        config.clone(),
    )?;
    assert_quiet_success(&set_user);
    assert_run_metadata(
        &set_user,
        &harness,
        &tmux_binary,
        &["set", "-g", "@my-user-opt", "hello-world"],
        &expected_overrides,
    );

    // Show the user option
    let show_user = harness.run_pair_with(
        &tmux_binary,
        &["show", "-gv", "@my-user-opt"],
        config.clone(),
    )?;
    assert_exact_tmux_compat(&show_user);
    assert_run_metadata(
        &show_user,
        &harness,
        &tmux_binary,
        &["show", "-gv", "@my-user-opt"],
        &expected_overrides,
    );
    assert_eq!(show_user.rmux.stdout_string().trim(), "hello-world");

    // Display via format that references user option
    let display_user = harness.run_pair_with(
        &tmux_binary,
        &["display-message", "-p", "opt=#{@my-user-opt}"],
        config,
    )?;
    assert_exact_tmux_compat(&display_user);
    assert_run_metadata(
        &display_user,
        &harness,
        &tmux_binary,
        &["display-message", "-p", "opt=#{@my-user-opt}"],
        &expected_overrides,
    );
    assert_eq!(display_user.tmux.stdout_string().trim(), "opt=hello-world");
    assert_eq!(display_user.rmux.stdout_string().trim(), "opt=hello-world");

    Ok(())
}

#[test]
fn tmux_compat_utf8_format_with_explicit_locale_when_frozen_tmux_is_available(
) -> Result<(), Box<dyn Error>> {
    let harness = TmuxCompatHarness::new("tmux-compat-utf8-format")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };

    // Coverage contract: UTF-8/width fixtures must set LC_CTYPE explicitly
    let config = tmux_compat_config()
        .with_env("LC_CTYPE", "C.UTF-8")
        .with_env("LC_ALL", "C.UTF-8")
        .with_env("TERM_PROGRAM", "tmux");
    let expected_overrides: EnvironmentOverrides = default_overrides(harness.tmpdir())
        .into_iter()
        .chain([
            (OsString::from("LC_CTYPE"), Some(OsString::from("C.UTF-8"))),
            (OsString::from("LC_ALL"), Some(OsString::from("C.UTF-8"))),
            (OsString::from("TERM_PROGRAM"), Some(OsString::from("tmux"))),
        ])
        .collect();

    let create = harness.run_pair_with(
        &tmux_binary,
        &["new-session", "-d", "-s", "alpha", "-x", "80", "-y", "24"],
        config.clone(),
    )?;
    assert_quiet_success(&create);

    let mut last = None;
    for _ in 0..100 {
        let display = harness.run_pair_with(
            &tmux_binary,
            &[
                "display-message",
                "-p",
                "-t",
                "alpha",
                "#{session_name}:#{window_name}:#{pane_width}",
            ],
            config.clone(),
        )?;
        if display.tmux.stdout == display.rmux.stdout
            && display.tmux.stderr == display.rmux.stderr
            && display.tmux.status_code == display.rmux.status_code
            && utf8_window_name_display_is_ready(&display.rmux.stdout_string())
        {
            assert_exact_tmux_compat(&display);
            assert_run_metadata(
                &display,
                &harness,
                &tmux_binary,
                &[
                    "display-message",
                    "-p",
                    "-t",
                    "alpha",
                    "#{session_name}:#{window_name}:#{pane_width}",
                ],
                &expected_overrides,
            );
            return Ok(());
        }
        last = Some(display);
        std::thread::sleep(Duration::from_millis(20));
    }

    let display = last.expect("utf8 format compatibility was attempted");
    assert_exact_tmux_compat(&display);
    assert_run_metadata(
        &display,
        &harness,
        &tmux_binary,
        &[
            "display-message",
            "-p",
            "-t",
            "alpha",
            "#{session_name}:#{window_name}:#{pane_width}",
        ],
        &expected_overrides,
    );
    assert!(
        utf8_window_name_display_is_ready(&display.rmux.stdout_string()),
        "expected alpha:<window_name>:80 output, got {:?}",
        display.rmux.stdout_string()
    );

    Ok(())
}

#[test]
fn tmux_compat_linked_window_formats_and_names_when_frozen_tmux_is_available(
) -> Result<(), Box<dyn Error>> {
    let harness = TmuxCompatHarness::new("tmux-compat-linked-window-formats")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let config = tmux_compat_config();
    let expected_overrides = default_overrides(harness.tmpdir());

    let create_alpha = harness.run_pair_with(
        &tmux_binary,
        &["new-session", "-d", "-s", "alpha"],
        config.clone(),
    )?;
    assert_quiet_success(&create_alpha);
    assert_run_metadata(
        &create_alpha,
        &harness,
        &tmux_binary,
        &["new-session", "-d", "-s", "alpha"],
        &expected_overrides,
    );

    let create_beta = harness.run_pair_with(
        &tmux_binary,
        &["new-session", "-d", "-s", "beta"],
        config.clone(),
    )?;
    assert_quiet_success(&create_beta);
    assert_run_metadata(
        &create_beta,
        &harness,
        &tmux_binary,
        &["new-session", "-d", "-s", "beta"],
        &expected_overrides,
    );

    let rename_alpha = harness.run_pair_with(
        &tmux_binary,
        &["rename-window", "-t", "alpha:0", "source"],
        config.clone(),
    )?;
    assert_quiet_success(&rename_alpha);
    assert_run_metadata(
        &rename_alpha,
        &harness,
        &tmux_binary,
        &["rename-window", "-t", "alpha:0", "source"],
        &expected_overrides,
    );

    let rename_beta = harness.run_pair_with(
        &tmux_binary,
        &["rename-window", "-t", "beta:0", "keep0"],
        config.clone(),
    )?;
    assert_quiet_success(&rename_beta);
    assert_run_metadata(
        &rename_beta,
        &harness,
        &tmux_binary,
        &["rename-window", "-t", "beta:0", "keep0"],
        &expected_overrides,
    );

    let link = harness.run_pair_with(
        &tmux_binary,
        &["link-window", "-s", "alpha:0", "-t", "beta:1"],
        config.clone(),
    )?;
    assert_exact_tmux_compat(&link);
    assert_run_metadata(
        &link,
        &harness,
        &tmux_binary,
        &["link-window", "-s", "alpha:0", "-t", "beta:1"],
        &expected_overrides,
    );

    let list_windows = harness.run_pair_with(
        &tmux_binary,
        &[
            "list-windows",
            "-t",
            "beta",
            "-F",
            "#{session_name}:#{window_index}:#{window_name}:#{window_linked}:#{window_linked_sessions}:#{window_linked_sessions_list}",
        ],
        config.clone(),
    )?;
    assert_exact_tmux_compat(&list_windows);
    assert_run_metadata(
        &list_windows,
        &harness,
        &tmux_binary,
        &[
            "list-windows",
            "-t",
            "beta",
            "-F",
            "#{session_name}:#{window_index}:#{window_name}:#{window_linked}:#{window_linked_sessions}:#{window_linked_sessions_list}",
        ],
        &expected_overrides,
    );
    assert!(
        list_windows
            .rmux
            .stdout_string()
            .contains("beta:1:source:1:2:alpha,beta"),
        "expected linked list-windows output, got {:?}",
        list_windows.rmux.stdout_string()
    );

    let rename = harness.run_pair_with(
        &tmux_binary,
        &["rename-window", "-t", "beta:1", "logs"],
        config.clone(),
    )?;
    assert_quiet_success(&rename);
    assert_run_metadata(
        &rename,
        &harness,
        &tmux_binary,
        &["rename-window", "-t", "beta:1", "logs"],
        &expected_overrides,
    );

    let display_linked = harness.run_pair_with(
        &tmux_binary,
        &[
            "display-message",
            "-p",
            "-t",
            "alpha:0",
            "#{window_name}:#{window_linked}:#{window_linked_sessions}:#{window_linked_sessions_list}",
        ],
        config.clone(),
    )?;
    assert_exact_tmux_compat(&display_linked);
    assert_run_metadata(
        &display_linked,
        &harness,
        &tmux_binary,
        &[
            "display-message",
            "-p",
            "-t",
            "alpha:0",
            "#{window_name}:#{window_linked}:#{window_linked_sessions}:#{window_linked_sessions_list}",
        ],
        &expected_overrides,
    );
    assert!(
        display_linked
            .rmux
            .stdout_string()
            .trim_end()
            .starts_with("logs:1:2:alpha,beta"),
        "expected linked display-message output, got {:?}",
        display_linked.rmux.stdout_string()
    );

    let unlink = harness.run_pair_with(
        &tmux_binary,
        &["unlink-window", "-t", "beta:1"],
        config.clone(),
    )?;
    assert_exact_tmux_compat(&unlink);
    assert_run_metadata(
        &unlink,
        &harness,
        &tmux_binary,
        &["unlink-window", "-t", "beta:1"],
        &expected_overrides,
    );

    let display_unlinked = harness.run_pair_with(
        &tmux_binary,
        &[
            "display-message",
            "-p",
            "-t",
            "alpha:0",
            "#{window_name}:#{window_linked}:#{window_linked_sessions}:#{window_linked_sessions_list}",
        ],
        config,
    )?;
    assert_exact_tmux_compat(&display_unlinked);
    assert_run_metadata(
        &display_unlinked,
        &harness,
        &tmux_binary,
        &[
            "display-message",
            "-p",
            "-t",
            "alpha:0",
            "#{window_name}:#{window_linked}:#{window_linked_sessions}:#{window_linked_sessions_list}",
        ],
        &expected_overrides,
    );
    assert_eq!(
        display_unlinked.rmux.stdout_string().trim(),
        "logs:0:1:alpha"
    );

    Ok(())
}

#[test]
fn tmux_compat_source_file_and_config_path_when_frozen_tmux_is_available(
) -> Result<(), Box<dyn Error>> {
    let harness = TmuxCompatHarness::new("tmux-compat-source-file")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let (config, expected_overrides) = config_with_clean_homes(&harness)?;

    // Write a config file that sets a user option
    let conf_path = harness.tmpdir().join("test.conf");
    fs::write(&conf_path, "set-option -g @sourced-opt loaded\n")?;
    let conf_str = conf_path.to_string_lossy().into_owned();

    let create = harness.run_pair_with(
        &tmux_binary,
        &["new-session", "-d", "-s", "alpha"],
        config.clone(),
    )?;
    assert_quiet_success(&create);

    // Source the file
    let source =
        harness.run_pair_with(&tmux_binary, &["source-file", &conf_str], config.clone())?;
    assert_quiet_success(&source);
    assert_run_metadata(
        &source,
        &harness,
        &tmux_binary,
        &["source-file", &conf_str],
        &expected_overrides,
    );

    // Verify the sourced option
    let show = harness.run_pair_with(&tmux_binary, &["show", "-gv", "@sourced-opt"], config)?;
    assert_exact_tmux_compat(&show);
    assert_eq!(show.rmux.stdout_string().trim(), "loaded");

    Ok(())
}

#[test]
fn tmux_compat_nested_attach_session_inside_tmux_uses_switch_client_surface(
) -> Result<(), Box<dyn Error>> {
    // Cluster J nested-TMUX coverage: the explicit `-f <missing> new-session -d`
    // row is the designated 0.1.0 failure check for this cluster. This row
    // closes the still-untested nested-attach half without pretending that it
    // is itself the release-baseline red case.
    let harness = TmuxCompatHarness::new("tmux-compat-nested-attach-switch-client")?;
    let (config, expected_overrides) = config_with_clean_homes(&harness)?;

    let create = harness.run_rmux_with(&["new-session", "-d", "-s", "alpha"], &config)?;
    assert_rmux_metadata(
        &create,
        &harness,
        &["new-session", "-d", "-s", "alpha"],
        &expected_overrides,
    );
    assert_eq!(create.status_code, Some(0));
    assert!(!create.timed_out);
    assert!(create.stdout.is_empty());
    assert!(create.stderr.is_empty());

    let nested_tmux = format!("{},1,0", harness.rmux_socket_path().display());
    let nested_attach = harness.run_rmux_with(
        &["attach-session", "-t", "alpha"],
        &config.clone().with_env("RMUX", nested_tmux.as_str()),
    )?;
    assert_eq!(nested_attach.status_code, Some(1));
    assert!(!nested_attach.timed_out);
    assert!(nested_attach.stdout.is_empty());
    assert_eq!(
        nested_attach.stderr_string(),
        "server error: switch-client requires an attached client\n"
    );

    let still_present = harness.run_rmux_with(&["has-session", "-t", "alpha"], &config)?;
    assert_rmux_metadata(
        &still_present,
        &harness,
        &["has-session", "-t", "alpha"],
        &expected_overrides,
    );
    assert_eq!(still_present.status_code, Some(0));
    assert!(!still_present.timed_out);
    assert!(still_present.stdout.is_empty());
    assert!(still_present.stderr.is_empty());

    Ok(())
}

#[test]
fn prefix_pane_right_records_cluster_b_baseline_trace_when_frozen_tmux_is_available(
) -> Result<(), Box<dyn Error>> {
    let harness = TmuxCompatHarness::new("cluster-b-prefix-pane-right")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let _guard = pty_tmux_compat_lock()
        .lock()
        .expect("pty compatibility lock");
    let deadline = ClusterBDeadline::new();
    let config = cluster_b_config();

    cluster_b_new_session(&harness, &tmux_binary, config.clone(), &deadline)?;
    let split = harness.run_pair_with(
        &tmux_binary,
        &["split-window", "-h", "-t", "alpha"],
        config.clone(),
    )?;
    assert_quiet_success(&split);
    let select_zero = harness.run_pair_with(
        &tmux_binary,
        &["select-pane", "-t", "alpha:0.0"],
        config.clone(),
    )?;
    assert_quiet_success(&select_zero);
    let cat = harness.run_pair_with(
        &tmux_binary,
        &["send-keys", "-t", "alpha:0.0", "cat -v", "Enter"],
        config.clone(),
    )?;
    assert_quiet_success(&cat);

    let mut rmux_attach = spawn_rmux_attached_input_client(&harness, "alpha")?;
    let mut tmux_attach = spawn_tmux_attached_input_client(&harness, &tmux_binary, "alpha")?;
    wait_for_cluster_b_clients(&harness, &tmux_binary, config.clone(), &deadline)?;
    write_attached_keys(&mut rmux_attach, b"\x02\x02", &deadline)?;
    write_attached_keys(&mut tmux_attach, b"\x02\x02", &deadline)?;
    write_attached_keys(&mut rmux_attach, b"x", &deadline)?;
    write_attached_keys(&mut tmux_attach, b"x", &deadline)?;
    std::thread::sleep(Duration::from_millis(150));

    let send_prefix_capture = cluster_b_capture_pair(
        &harness,
        &tmux_binary,
        "alpha:0.0",
        config.clone(),
        &deadline,
    )?;
    assert_eq!(
        send_prefix_capture
            .rmux
            .stdout_string()
            .matches("^B")
            .count(),
        1,
        "prefix_pane_right rmux capture should record one C-b byte, got {:?}",
        send_prefix_capture.rmux.stdout_string()
    );
    assert_eq!(
        send_prefix_capture
            .tmux
            .stdout_string()
            .matches("^B")
            .count(),
        1,
        "prefix_pane_right tmux capture should record one C-b byte, got {:?}",
        send_prefix_capture.tmux.stdout_string()
    );
    assert!(
        send_prefix_capture.rmux.stdout_string().contains("^Bx"),
        "prefix_pane_right rmux capture should return to root input after send-prefix, got {:?}",
        send_prefix_capture.rmux.stdout_string()
    );
    assert!(
        send_prefix_capture.tmux.stdout_string().contains("^Bx"),
        "prefix_pane_right tmux capture should return to root input after send-prefix, got {:?}",
        send_prefix_capture.tmux.stdout_string()
    );

    write_attached_keys(&mut rmux_attach, b"\x02\x1b[C", &deadline)?;
    write_attached_keys(&mut tmux_attach, b"\x02\x1b[C", &deadline)?;
    let panes = wait_for_cluster_b_pair(
        &harness,
        &tmux_binary,
        &[
            "list-panes",
            "-t",
            "alpha",
            "-F",
            "#{pane_index}:#{pane_active}",
        ],
        config.clone(),
        &deadline,
        |run| run.rmux.stdout_string().contains("1:1") && run.tmux.stdout_string().contains("1:1"),
    )?;
    rmux_attach.assert_running("rmux")?;
    tmux_attach.assert_running("tmux")?;
    assert_eq!(panes.rmux.stdout_string(), "0:0\n1:1\n");
    assert_eq!(panes.tmux.stdout_string(), "0:0\n1:1\n");

    drop(rmux_attach);
    drop(tmux_attach);

    let new_window = harness.run_pair_with(
        &tmux_binary,
        &["new-window", "-d", "-t", "alpha"],
        config.clone(),
    )?;
    assert_quiet_success(&new_window);
    let select_window_zero = harness.run_pair_with(
        &tmux_binary,
        &["select-window", "-t", "alpha:0"],
        config.clone(),
    )?;
    assert_quiet_success(&select_window_zero);
    let mut rmux_attach = spawn_rmux_attached_input_client(&harness, "alpha")?;
    let mut tmux_attach = spawn_tmux_attached_input_client(&harness, &tmux_binary, "alpha")?;
    wait_for_cluster_b_clients(&harness, &tmux_binary, config.clone(), &deadline)?;
    write_attached_keys(&mut rmux_attach, b"\x02n", &deadline)?;
    write_attached_keys(&mut tmux_attach, b"\x02n", &deadline)?;
    let windows = wait_for_cluster_b_pair(
        &harness,
        &tmux_binary,
        &[
            "list-windows",
            "-t",
            "alpha",
            "-F",
            "#{window_index}:#{window_active}",
        ],
        config,
        &deadline,
        |run| run.rmux.stdout_string().contains("1:1") && run.tmux.stdout_string().contains("1:1"),
    )?;
    assert_eq!(windows.rmux.stdout_string(), "0:0\n1:1\n");
    assert_eq!(windows.tmux.stdout_string(), "0:0\n1:1\n");

    drop(rmux_attach);
    drop(tmux_attach);
    shutdown_cluster_b_rmux(&harness)?;
    Ok(())
}

#[test]
fn prefix_q_display_panes_timeout_matches_tmux_when_frozen_tmux_is_available(
) -> Result<(), Box<dyn Error>> {
    let harness = TmuxCompatHarness::new("cluster-b-prefix-q-timeout")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let _guard = pty_tmux_compat_lock()
        .lock()
        .expect("pty compatibility lock");
    let deadline = ClusterBDeadline::new();
    let config = cluster_b_config();

    cluster_b_new_session(&harness, &tmux_binary, config.clone(), &deadline)?;
    let split = harness.run_pair_with(
        &tmux_binary,
        &["split-window", "-h", "-t", "alpha"],
        config.clone(),
    )?;
    assert_quiet_success(&split);

    let mut rmux_attach = spawn_rmux_attached_input_client(&harness, "alpha")?;
    let mut tmux_attach = spawn_tmux_attached_input_client(&harness, &tmux_binary, "alpha")?;
    wait_for_cluster_b_clients(&harness, &tmux_binary, config, &deadline)?;

    std::thread::sleep(Duration::from_millis(200));
    let mut rmux_bytes = drain_pty(&mut rmux_attach)?;
    let mut tmux_bytes = drain_pty(&mut tmux_attach)?;

    write_attached_keys(&mut rmux_attach, b"\x02q", &deadline)?;
    write_attached_keys(&mut tmux_attach, b"\x02q", &deadline)?;

    std::thread::sleep(Duration::from_millis(200));
    rmux_bytes.extend(drain_pty(&mut rmux_attach)?);
    tmux_bytes.extend(drain_pty(&mut tmux_attach)?);

    let rmux_early = render_transcript(&rmux_bytes, 80, 24);
    let tmux_early = render_transcript(&tmux_bytes, 80, 24);
    assert!(
        display_panes_overlay_visible(&rmux_early),
        "rmux should show display-panes shortly after prefix q, got {rmux_early:?}"
    );
    assert!(
        display_panes_overlay_visible(&tmux_early),
        "tmux should show display-panes shortly after prefix q, got {tmux_early:?}"
    );

    std::thread::sleep(Duration::from_millis(1_300));
    rmux_bytes.extend(drain_pty(&mut rmux_attach)?);
    tmux_bytes.extend(drain_pty(&mut tmux_attach)?);

    let rmux_late = render_transcript(&rmux_bytes, 80, 24);
    let tmux_late = render_transcript(&tmux_bytes, 80, 24);
    assert_eq!(
        display_panes_overlay_visible(&tmux_late),
        display_panes_overlay_visible(&rmux_late),
        "display-panes timeout visibility diverged\n--- tmux ---\n{tmux_late}\n--- rmux ---\n{rmux_late}"
    );

    drop(rmux_attach);
    drop(tmux_attach);
    shutdown_cluster_b_rmux(&harness)?;
    Ok(())
}

#[test]
fn copy_mode_search_select_records_cluster_b_baseline_trace_when_frozen_tmux_is_available(
) -> Result<(), Box<dyn Error>> {
    let harness = TmuxCompatHarness::new("cluster-b-copy-mode-search-select")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let _guard = pty_tmux_compat_lock()
        .lock()
        .expect("pty compatibility lock");
    let deadline = ClusterBDeadline::new();
    let config = cluster_b_config();

    cluster_b_new_session(&harness, &tmux_binary, config.clone(), &deadline)?;
    let mode_keys = harness.run_pair_with(
        &tmux_binary,
        &["set-window-option", "-g", "mode-keys", "vi"],
        config.clone(),
    )?;
    assert_quiet_success(&mode_keys);
    let copy_mode = harness.run_pair_with(
        &tmux_binary,
        &["copy-mode", "-t", "alpha:0.0"],
        config.clone(),
    )?;
    assert_quiet_success(&copy_mode);

    let mut rmux_attach = spawn_rmux_attached_input_client(&harness, "alpha")?;
    let mut tmux_attach = spawn_tmux_attached_input_client(&harness, &tmux_binary, "alpha")?;
    wait_for_cluster_b_clients(&harness, &tmux_binary, config.clone(), &deadline)?;
    write_attached_keys(&mut rmux_attach, b"/P0-LINE-12\r \r", &deadline)?;
    write_attached_keys(&mut tmux_attach, b"/P0-LINE-12\r \r", &deadline)?;
    std::thread::sleep(Duration::from_millis(150));

    let after_select = cluster_b_capture_pair(
        &harness,
        &tmux_binary,
        "alpha:0.0",
        config.clone(),
        &deadline,
    )?;
    assert!(
        !after_select.rmux.stdout_string().contains("/P0-LINE-12"),
        "copy_mode_search_select rmux capture should consume attached search keys, got {:?}",
        after_select.rmux.stdout_string()
    );
    assert!(
        !after_select.tmux.stdout_string().contains("/P0-LINE-12"),
        "copy_mode_search_select tmux capture should consume attached search keys, got {:?}",
        after_select.tmux.stdout_string()
    );

    drop(rmux_attach);
    drop(tmux_attach);
    shutdown_cluster_b_rmux(&harness)?;
    Ok(())
}

#[test]
fn copy_mode_q_exit_records_cluster_b_baseline_trace_when_frozen_tmux_is_available(
) -> Result<(), Box<dyn Error>> {
    let harness = TmuxCompatHarness::new("cluster-b-copy-mode-q-exit")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let _guard = pty_tmux_compat_lock()
        .lock()
        .expect("pty compatibility lock");
    let deadline = ClusterBDeadline::new();
    let config = cluster_b_config();

    cluster_b_new_session(&harness, &tmux_binary, config.clone(), &deadline)?;
    let copy_mode = harness.run_pair_with(
        &tmux_binary,
        &["copy-mode", "-t", "alpha:0.0"],
        config.clone(),
    )?;
    assert_quiet_success(&copy_mode);
    let mut rmux_attach = spawn_rmux_attached_input_client(&harness, "alpha")?;
    let mut tmux_attach = spawn_tmux_attached_input_client(&harness, &tmux_binary, "alpha")?;
    wait_for_cluster_b_clients(&harness, &tmux_binary, config.clone(), &deadline)?;

    let before = wait_for_cluster_b_pair(
        &harness,
        &tmux_binary,
        &[
            "display-message",
            "-p",
            "-t",
            "alpha:0.0",
            "#{pane_in_mode}|#{pane_mode}",
        ],
        config.clone(),
        &deadline,
        |run| {
            run.rmux.stdout_string() == "1|copy-mode\n"
                && run.tmux.stdout_string() == "1|copy-mode\n"
        },
    )?;
    assert_eq!(before.rmux.stdout_string(), "1|copy-mode\n");
    assert_eq!(before.tmux.stdout_string(), "1|copy-mode\n");

    write_attached_keys(&mut rmux_attach, b"q", &deadline)?;
    write_attached_keys(&mut tmux_attach, b"q", &deadline)?;
    let after = wait_for_cluster_b_pair(
        &harness,
        &tmux_binary,
        &[
            "display-message",
            "-p",
            "-t",
            "alpha:0.0",
            "#{pane_in_mode}|#{pane_mode}",
        ],
        config.clone(),
        &deadline,
        |run| run.rmux.stdout_string() == "0|\n" && run.tmux.stdout_string() == "0|\n",
    )?;
    assert_eq!(after.rmux.stdout_string(), "0|\n");
    assert_eq!(after.tmux.stdout_string(), "0|\n");
    let after_exit_keys = cluster_b_capture_pair(
        &harness,
        &tmux_binary,
        "alpha:0.0",
        config.clone(),
        &deadline,
    )?;
    assert!(
        !after_exit_keys.rmux.stdout_string().contains("\nq"),
        "copy_mode_q_exit rmux capture should keep q out of pane input, got {:?}",
        after_exit_keys.rmux.stdout_string()
    );
    assert!(
        !after_exit_keys.tmux.stdout_string().contains("\nq"),
        "copy_mode_q_exit tmux capture should keep q out of pane input, got {:?}",
        after_exit_keys.tmux.stdout_string()
    );

    drop(rmux_attach);
    drop(tmux_attach);
    shutdown_cluster_b_rmux(&harness)?;
    Ok(())
}

#[test]
fn copy_mode_escape_exit_records_cluster_b_baseline_trace_when_frozen_tmux_is_available(
) -> Result<(), Box<dyn Error>> {
    let harness = TmuxCompatHarness::new("cluster-b-copy-mode-escape-exit")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let _guard = pty_tmux_compat_lock()
        .lock()
        .expect("pty compatibility lock");
    let deadline = ClusterBDeadline::new();
    let config = cluster_b_config();

    cluster_b_new_session(&harness, &tmux_binary, config.clone(), &deadline)?;
    let copy_mode = harness.run_pair_with(
        &tmux_binary,
        &["copy-mode", "-t", "alpha:0.0"],
        config.clone(),
    )?;
    assert_quiet_success(&copy_mode);
    let mut rmux_attach = spawn_rmux_attached_input_client(&harness, "alpha")?;
    let mut tmux_attach = spawn_tmux_attached_input_client(&harness, &tmux_binary, "alpha")?;
    wait_for_cluster_b_clients(&harness, &tmux_binary, config.clone(), &deadline)?;

    let before = wait_for_cluster_b_pair(
        &harness,
        &tmux_binary,
        &[
            "display-message",
            "-p",
            "-t",
            "alpha:0.0",
            "#{pane_in_mode}|#{pane_mode}",
        ],
        config.clone(),
        &deadline,
        |run| {
            run.rmux.stdout_string() == "1|copy-mode\n"
                && run.tmux.stdout_string() == "1|copy-mode\n"
        },
    )?;
    assert_eq!(before.rmux.stdout_string(), "1|copy-mode\n");
    assert_eq!(before.tmux.stdout_string(), "1|copy-mode\n");

    write_attached_keys(&mut rmux_attach, b"\x1b", &deadline)?;
    write_attached_keys(&mut tmux_attach, b"\x1b", &deadline)?;
    let after = wait_for_cluster_b_pair(
        &harness,
        &tmux_binary,
        &[
            "display-message",
            "-p",
            "-t",
            "alpha:0.0",
            "#{pane_in_mode}|#{pane_mode}",
        ],
        config.clone(),
        &deadline,
        |run| run.rmux.stdout_string() == "0|\n" && run.tmux.stdout_string() == "0|\n",
    )?;
    assert_eq!(after.rmux.stdout_string(), "0|\n");
    assert_eq!(after.tmux.stdout_string(), "0|\n");

    rmux_attach.assert_running("rmux")?;
    tmux_attach.assert_running("tmux")?;
    drop(rmux_attach);
    drop(tmux_attach);
    shutdown_cluster_b_rmux(&harness)?;
    Ok(())
}

#[test]
fn copy_mode_u_render_records_cluster_b_baseline_trace_when_frozen_tmux_is_available(
) -> Result<(), Box<dyn Error>> {
    let harness = TmuxCompatHarness::new("cluster-b-copy-mode-u-render")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let _guard = pty_tmux_compat_lock()
        .lock()
        .expect("pty compatibility lock");
    let deadline = ClusterBDeadline::new();
    let config = cluster_b_config();

    cluster_b_new_session(&harness, &tmux_binary, config.clone(), &deadline)?;
    let mut rmux_attach = spawn_rmux_attached_input_client(&harness, "alpha")?;
    let mut tmux_attach = spawn_tmux_attached_input_client(&harness, &tmux_binary, "alpha")?;
    wait_for_cluster_b_clients(&harness, &tmux_binary, config.clone(), &deadline)?;
    let _ = drain_pty(&mut rmux_attach)?;
    let _ = drain_pty(&mut tmux_attach)?;

    let copy_mode_u = harness.run_pair_with(
        &tmux_binary,
        &["copy-mode", "-u", "-t", "alpha:0.0"],
        config.clone(),
    )?;
    assert_quiet_success(&copy_mode_u);
    std::thread::sleep(Duration::from_millis(250));
    deadline.check()?;
    let rmux_render = render_cells(&drain_pty(&mut rmux_attach)?, 80, 24).join("\n");
    let tmux_render = render_cells(&drain_pty(&mut tmux_attach)?, 80, 24).join("\n");
    let capture = cluster_b_capture_pair(
        &harness,
        &tmux_binary,
        "alpha:0.0",
        config.clone(),
        &deadline,
    )?;
    let mode = harness.run_pair_with(
        &tmux_binary,
        &[
            "display-message",
            "-p",
            "-t",
            "alpha:0.0",
            "#{pane_in_mode}|#{pane_mode}",
        ],
        config,
    )?;
    assert_eq!(mode.rmux.stdout_string(), "1|copy-mode\n");
    assert_eq!(mode.tmux.stdout_string(), "1|copy-mode\n");
    assert!(
        capture.rmux.stdout_string().contains("P0-LINE-12")
            && capture.tmux.stdout_string().contains("P0-LINE-12"),
        "copy_mode_u_render capture-pane should keep scrollback on both servers; rmux={:?} tmux={:?}",
        capture.rmux.stdout_string(),
        capture.tmux.stdout_string()
    );
    assert!(
        tmux_render.contains("P0-LINE-12"),
        "copy_mode_u_render tmux attached render should show scrollback, got {tmux_render:?}"
    );
    assert!(
        rmux_render.contains("P0-LINE-12"),
        "copy_mode_u_render rmux attached render should show scrollback, got {rmux_render:?}"
    );

    drop(rmux_attach);
    drop(tmux_attach);
    shutdown_cluster_b_rmux(&harness)?;
    Ok(())
}

#[test]
fn choose_tree_window_records_cluster_b_baseline_trace_when_frozen_tmux_is_available(
) -> Result<(), Box<dyn Error>> {
    let harness = TmuxCompatHarness::new("cluster-b-choose-tree-window")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let _guard = pty_tmux_compat_lock()
        .lock()
        .expect("pty compatibility lock");
    let deadline = ClusterBDeadline::new();
    let config = cluster_b_config();

    cluster_b_new_session(&harness, &tmux_binary, config.clone(), &deadline)?;
    let new_window = harness.run_pair_with(
        &tmux_binary,
        &["new-window", "-d", "-t", "alpha", "-n", "w1"],
        config.clone(),
    )?;
    assert_quiet_success(&new_window);
    let mut rmux_attach = spawn_rmux_attached_input_client(&harness, "alpha")?;
    let mut tmux_attach = spawn_tmux_attached_input_client(&harness, &tmux_binary, "alpha")?;
    wait_for_cluster_b_clients(&harness, &tmux_binary, config.clone(), &deadline)?;

    let choose_tree =
        harness.run_pair_with(&tmux_binary, &["choose-tree", "-Zw"], config.clone())?;
    assert_quiet_success(&choose_tree);
    std::thread::sleep(Duration::from_millis(150));
    let tree_capture = cluster_b_capture_pair(
        &harness,
        &tmux_binary,
        "alpha:0.0",
        config.clone(),
        &deadline,
    )?;
    write_attached_keys(&mut rmux_attach, b"\x0e\r", &deadline)?;
    write_attached_keys(&mut tmux_attach, b"\x0e\r", &deadline)?;

    std::thread::sleep(Duration::from_millis(300));
    deadline.check()?;
    let active_window = harness.run_pair_with(
        &tmux_binary,
        &["display-message", "-p", "#{window_index}:#{window_name}"],
        config,
    )?;
    rmux_attach.assert_running("rmux")?;
    tmux_attach.assert_running("tmux")?;
    assert!(tree_capture.rmux.status_code == Some(0) && tree_capture.tmux.status_code == Some(0));
    assert_eq!(active_window.rmux.stdout_string(), "1:w1\n");
    assert_eq!(active_window.tmux.stdout_string(), "1:w1\n");

    drop(rmux_attach);
    drop(tmux_attach);
    shutdown_cluster_b_rmux(&harness)?;
    Ok(())
}

#[test]
fn tmux_compat_copy_mode_and_control_mode_exact_surfaces_when_frozen_tmux_is_available(
) -> Result<(), Box<dyn Error>> {
    let harness = TmuxCompatHarness::new("tmux-compat-copy-control-exact")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let config = tmux_compat_config();
    let expected_overrides = default_overrides(harness.tmpdir());

    let create = harness.run_pair_with(
        &tmux_binary,
        &["new-session", "-d", "-s", "alpha"],
        config.clone(),
    )?;
    assert_quiet_success(&create);
    assert_run_metadata(
        &create,
        &harness,
        &tmux_binary,
        &["new-session", "-d", "-s", "alpha"],
        &expected_overrides,
    );

    let copy_mode =
        harness.run_pair_with(&tmux_binary, &["copy-mode", "-t", "alpha:0.0"], config)?;
    assert_exact_tmux_compat(&copy_mode);
    assert_run_metadata(
        &copy_mode,
        &harness,
        &tmux_binary,
        &["copy-mode", "-t", "alpha:0.0"],
        &expected_overrides,
    );

    let usage = harness.run_rmux(&["-h"])?;
    assert!(
        usage.stdout_string().contains('C'),
        "rmux usage should document C flag for control mode: {:?}",
        usage.stdout_string()
    );

    Ok(())
}

#[test]
fn tmux_compat_hook_allow_list_show_hooks_and_prefix_binding_surface_on_rmux_release_head(
) -> Result<(), Box<dyn Error>> {
    // Cluster L shipped a deliberately rmux-specific surface: rejected hooks
    // are trimmed to the dispatch allow-list, `show-hooks` renders tmux-style
    // `hook[index] command`, and `list-keys -T prefix C-b` preserves the
    // global table alignment. Frozen tmux is not the authority for the exact
    // `list-keys -T prefix C-b` invocation because tmux emits no row there; the
    // release check for this cluster is rmux 0.1.0 vs current HEAD.
    let harness = TmuxCompatHarness::new("tmux-compat-hook-allow-list-release-head")?;
    let (config, expected_overrides) = config_with_clean_homes(&harness)?;

    let create = harness.run_rmux_with(&["new-session", "-d", "-s", "alpha"], &config)?;
    assert_rmux_metadata(
        &create,
        &harness,
        &["new-session", "-d", "-s", "alpha"],
        &expected_overrides,
    );
    assert_eq!(create.status_code, Some(0));
    assert!(!create.timed_out);
    assert!(create.stdout.is_empty());
    assert!(create.stderr.is_empty());

    let allowed = harness.run_rmux_with(
        &[
            "set-hook",
            "-t",
            "alpha",
            "client-attached",
            "display-message hi",
        ],
        &config,
    )?;
    assert_rmux_metadata(
        &allowed,
        &harness,
        &[
            "set-hook",
            "-t",
            "alpha",
            "client-attached",
            "display-message hi",
        ],
        &expected_overrides,
    );
    assert_eq!(allowed.status_code, Some(0));
    assert!(!allowed.timed_out);
    assert!(allowed.stdout.is_empty());
    assert!(allowed.stderr.is_empty());

    let rejected = harness.run_rmux_with(
        &["set-hook", "-g", "window-resized", "display-message hi"],
        &config,
    )?;
    assert_rmux_metadata(
        &rejected,
        &harness,
        &["set-hook", "-g", "window-resized", "display-message hi"],
        &expected_overrides,
    );
    assert_eq!(rejected.status_code, Some(1));
    assert!(!rejected.timed_out);
    assert!(rejected.stdout.is_empty());
    assert_eq!(
        rejected.stderr_string(),
        "window-resized is not supported: rmux does not dispatch this hook\n"
    );

    let show_hooks =
        harness.run_rmux_with(&["show-hooks", "-t", "alpha", "client-attached"], &config)?;
    assert_rmux_metadata(
        &show_hooks,
        &harness,
        &["show-hooks", "-t", "alpha", "client-attached"],
        &expected_overrides,
    );
    assert_eq!(show_hooks.status_code, Some(0));
    assert!(!show_hooks.timed_out);
    assert_eq!(
        show_hooks.stdout_string(),
        "client-attached[0] display-message hi\n"
    );
    assert!(show_hooks.stderr.is_empty());

    let list_keys = harness.run_rmux_with(&["list-keys", "-T", "prefix", "C-b"], &config)?;
    assert_rmux_metadata(
        &list_keys,
        &harness,
        &["list-keys", "-T", "prefix", "C-b"],
        &expected_overrides,
    );
    assert_eq!(list_keys.status_code, Some(0));
    assert!(!list_keys.timed_out);
    assert_eq!(
        list_keys.stdout_string(),
        "bind-key -T prefix C-b send-prefix\n"
    );
    assert!(list_keys.stderr.is_empty());

    Ok(())
}

#[test]
fn tmux_compat_key_tables_and_list_keys_exact_surface_when_frozen_tmux_is_available(
) -> Result<(), Box<dyn Error>> {
    let harness = TmuxCompatHarness::new("tmux-compat-key-tables")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let config = tmux_compat_config();
    let expected_overrides = default_overrides(harness.tmpdir());

    let create = harness.run_pair_with(
        &tmux_binary,
        &["new-session", "-d", "-s", "alpha"],
        config.clone(),
    )?;
    assert_quiet_success(&create);
    assert_run_metadata(
        &create,
        &harness,
        &tmux_binary,
        &["new-session", "-d", "-s", "alpha"],
        &expected_overrides,
    );

    let list_keys = harness.run_pair_with(
        &tmux_binary,
        &[
            "list-keys",
            "-F",
            "#{key_table}:#{key_string}:#{key_repeat}",
            "-T",
            "prefix",
        ],
        config,
    )?;
    assert_eq!(list_keys.tmux.status_code, list_keys.rmux.status_code);
    assert_eq!(list_keys.tmux.timed_out, list_keys.rmux.timed_out);
    assert_eq!(list_keys.tmux.stderr, list_keys.rmux.stderr);
    assert_eq!(
        drop_frozen_mirrored_layout_bindings(&list_keys.tmux.stdout),
        list_keys.rmux.stdout
    );
    assert_run_metadata(
        &list_keys,
        &harness,
        &tmux_binary,
        &[
            "list-keys",
            "-F",
            "#{key_table}:#{key_string}:#{key_repeat}",
            "-T",
            "prefix",
        ],
        &expected_overrides,
    );
    assert!(
        list_keys
            .rmux
            .stdout_string()
            .starts_with("prefix:Space:0\n"),
        "expected list-keys formatter compatibility output to enumerate prefix bindings, got {:?}",
        list_keys.rmux.stdout_string()
    );

    Ok(())
}

#[test]
fn tmux_compat_show_messages_exact_surface_when_frozen_tmux_is_available(
) -> Result<(), Box<dyn Error>> {
    let harness = TmuxCompatHarness::new("tmux-compat-alerts-message-log")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let config = tmux_compat_config();
    let expected_overrides = default_overrides(harness.tmpdir());

    let create = harness.run_pair_with(
        &tmux_binary,
        &["new-session", "-d", "-s", "alpha"],
        config.clone(),
    )?;
    assert_quiet_success(&create);
    assert_run_metadata(
        &create,
        &harness,
        &tmux_binary,
        &["new-session", "-d", "-s", "alpha"],
        &expected_overrides,
    );

    let show_messages =
        harness.run_pair_with(&tmux_binary, &["show-messages", "-J", "-T"], config)?;
    assert_exact_tmux_compat(&show_messages);
    assert_run_metadata(
        &show_messages,
        &harness,
        &tmux_binary,
        &["show-messages", "-J", "-T"],
        &expected_overrides,
    );

    Ok(())
}

#[test]
fn tmux_compat_prompt_target_client_error_surface_when_frozen_tmux_is_available(
) -> Result<(), Box<dyn Error>> {
    let harness = TmuxCompatHarness::new("tmux-compat-prompt-target-client")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let config = tmux_compat_config();
    let expected_overrides = default_overrides(harness.tmpdir());

    let create = harness.run_pair_with(
        &tmux_binary,
        &["new-session", "-d", "-s", "alpha"],
        config.clone(),
    )?;
    assert_quiet_success(&create);
    assert_run_metadata(
        &create,
        &harness,
        &tmux_binary,
        &["new-session", "-d", "-s", "alpha"],
        &expected_overrides,
    );

    let command_prompt = harness.run_pair_with(
        &tmux_binary,
        &["command-prompt", "-t", "99999", "display-message hi"],
        config.clone(),
    )?;
    assert_exact_tmux_compat(&command_prompt);
    assert_run_metadata(
        &command_prompt,
        &harness,
        &tmux_binary,
        &["command-prompt", "-t", "99999", "display-message hi"],
        &expected_overrides,
    );

    let confirm_before = harness.run_pair_with(
        &tmux_binary,
        &["confirm-before", "-t", "99999", "display-message confirmed"],
        config,
    )?;
    assert_exact_tmux_compat(&confirm_before);
    assert_run_metadata(
        &confirm_before,
        &harness,
        &tmux_binary,
        &["confirm-before", "-t", "99999", "display-message confirmed"],
        &expected_overrides,
    );

    Ok(())
}

#[test]
fn tmux_compat_step2c_command_alias_surface_when_frozen_tmux_is_available(
) -> Result<(), Box<dyn Error>> {
    let harness = TmuxCompatHarness::new("tmux-compat-step2c-command-surface")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let config = tmux_compat_config();
    let expected_overrides = default_overrides(harness.tmpdir());

    let run = harness.run_pair_with(&tmux_binary, &["list-commands"], config)?;
    assert_run_metadata(
        &run,
        &harness,
        &tmux_binary,
        &["list-commands"],
        &expected_overrides,
    );
    assert_success_without_stderr(&run);

    let tmux_commands = sorted_first_words(&run.tmux.stdout_string());
    let rmux_commands = sorted_first_words(&run.rmux.stdout_string());
    for command in [
        "clear-prompt-history",
        "display-menu",
        "display-popup",
        "show-prompt-history",
    ] {
        assert!(
            tmux_commands.contains(&command.to_owned()),
            "tmux list-commands should include {command}: {:?}",
            run.tmux.stdout_string()
        );
        assert!(
            rmux_commands.contains(&command.to_owned()),
            "rmux list-commands should include {command}: {:?}",
            run.rmux.stdout_string()
        );
    }

    let rmux_surface = run.rmux.stdout_string();
    for alias in ["clearphist", "menu", "popup", "showphist"] {
        assert!(
            rmux_surface.contains(alias),
            "rmux list-commands should expose alias {alias}: {rmux_surface:?}"
        );
    }

    Ok(())
}

/// Cluster I (error/exit matrix alignment) live row pinning.
///
/// The durable matrix lives in
/// `tests/reference/cluster_i_error_exit_matrix.yaml`; this test is the
/// live compatibility check for rows 1a/1b. Those rows flow through the existing
/// `ExitFailure` / `main.rs` stream-and-exit pipeline and reuse the
/// `RmuxError` Display shapes documented in
/// `crates/rmux-proto/src/error.rs`.
///
/// Rows 1a/1b are the baseline-failure check for this pass: on 0.1.0
/// the rmux stderr begins with `"server error: "` because both unlock
/// branches in `WaitForStore::unlock` constructed `RmuxError::Server(...)`;
/// on the continuation HEAD both branches use `RmuxError::Message(...)`
/// so the bytes match tmux exactly. `RmuxError` Display shapes in
/// `crates/rmux-proto/src/error.rs:33-37` are unchanged - the fix lands
/// at the construction site, per the Cluster I coverage contract.
#[test]
fn tmux_compat_wait_for_unlock_not_locked_channel_when_frozen_tmux_is_available(
) -> Result<(), Box<dyn Error>> {
    let harness = TmuxCompatHarness::new("tmux-compat-wait-for-unlock-unknown")?;
    let Some(tmux_binary) = frozen_tmux_or_skip(&harness)? else {
        return Ok(());
    };
    let config = tmux_compat_config();
    let expected_overrides = default_overrides(harness.tmpdir());

    let start_args = ["new-session", "-d", "-s", "alpha"];
    let start = harness.run_pair_with(&tmux_binary, &start_args, config.clone())?;
    assert_quiet_success(&start);
    assert_run_metadata(
        &start,
        &harness,
        &tmux_binary,
        &start_args,
        &expected_overrides,
    );

    // Cluster I row 1a: channel has never been seen by the wait-for
    // store, so `self.channels.get_mut(channel)` is None. tmux-observed
    // tuple is stdout="", stderr="channel unknownchan not locked\n",
    // exit=1. This is the designated baseline-failure row: on the 0.1.0
    // baseline the rmux stderr is prefixed with "server error: ", on
    // the continuation HEAD it matches tmux byte-for-byte.
    let unlock_unknown_args = ["wait-for", "-U", "unknownchan"];
    let unlock_unknown =
        harness.run_pair_with(&tmux_binary, &unlock_unknown_args, config.clone())?;
    assert_run_metadata(
        &unlock_unknown,
        &harness,
        &tmux_binary,
        &unlock_unknown_args,
        &expected_overrides,
    );
    assert_eq!(unlock_unknown.tmux.stdout, b"");
    assert_eq!(
        unlock_unknown.tmux.stderr_string(),
        "channel unknownchan not locked\n"
    );
    assert_eq!(unlock_unknown.tmux.status_code, Some(1));
    assert!(!unlock_unknown.tmux.timed_out);
    // Pin rmux independently so a future tmux-side byte shift cannot
    // silently pass via `assert_exact_tmux_compat` alone.
    assert_eq!(unlock_unknown.rmux.stdout, b"");
    assert_eq!(
        unlock_unknown.rmux.stderr_string(),
        "channel unknownchan not locked\n"
    );
    assert_eq!(unlock_unknown.rmux.status_code, Some(1));
    assert!(!unlock_unknown.rmux.timed_out);
    assert_exact_tmux_compat(&unlock_unknown);

    // Cluster I row 1b: signaling "signaled-chan" creates an entry in
    // the wait-for store with `woken=true, locked=false`. A subsequent
    // `wait-for -U signaled-chan` now hits the `!state.locked` branch
    // at `crates/rmux-server/src/wait_for.rs:213` rather than the
    // "channel absent from map" branch. Both branches must produce the
    // same bare tmux-compatible error text.
    let signal_args = ["wait-for", "-S", "signaled-chan"];
    let signal = harness.run_pair_with(&tmux_binary, &signal_args, config.clone())?;
    assert_run_metadata(
        &signal,
        &harness,
        &tmux_binary,
        &signal_args,
        &expected_overrides,
    );
    assert_quiet_success(&signal);

    let unlock_signaled_args = ["wait-for", "-U", "signaled-chan"];
    let unlock_signaled =
        harness.run_pair_with(&tmux_binary, &unlock_signaled_args, config.clone())?;
    assert_run_metadata(
        &unlock_signaled,
        &harness,
        &tmux_binary,
        &unlock_signaled_args,
        &expected_overrides,
    );
    assert_eq!(unlock_signaled.tmux.stdout, b"");
    assert_eq!(
        unlock_signaled.tmux.stderr_string(),
        "channel signaled-chan not locked\n"
    );
    assert_eq!(unlock_signaled.tmux.status_code, Some(1));
    assert!(!unlock_signaled.tmux.timed_out);
    assert_eq!(unlock_signaled.rmux.stdout, b"");
    assert_eq!(
        unlock_signaled.rmux.stderr_string(),
        "channel signaled-chan not locked\n"
    );
    assert_eq!(unlock_signaled.rmux.status_code, Some(1));
    assert!(!unlock_signaled.rmux.timed_out);
    assert_exact_tmux_compat(&unlock_signaled);

    Ok(())
}

fn sorted_first_words(output: &str) -> Vec<String> {
    let mut words: Vec<String> = output
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(ToOwned::to_owned)
        .collect();
    words.sort();
    words.dedup();
    words
}

fn assert_matching_line(run: &TmuxCompatRun, prefix: &str) -> String {
    let tmux_line = line_with_prefix(&run.tmux.stdout_string(), prefix);
    let rmux_line = line_with_prefix(&run.rmux.stdout_string(), prefix);
    assert_eq!(tmux_line, rmux_line);
    rmux_line
}

fn line_with_prefix(output: &str, prefix: &str) -> String {
    output
        .lines()
        .find(|line| line.starts_with(prefix))
        .unwrap_or_else(|| panic!("missing line with prefix {prefix:?} in output {output:?}"))
        .to_owned()
}
