// ===========================================================================
// dyson init — initialize ~/.dyson with default config and workspace.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Creates the Dyson directory structure, writes a default config file,
//   optionally imports an existing filesystem workspace, installs the binary
//   to PATH, and optionally sets up a systemd service.
//
// Directory layout (~/.dyson/):
//
//   ~/.dyson/
//     dyson.json           — main config file
//     bin/dyson            — installed binary copy
//     workspace/
//       SOUL.md            — personality
//       IDENTITY.md        — who the agent is
//       MEMORY.md          — long-term memory
//       AGENTS.md          — operating procedures
//       HEARTBEAT.md       — periodic tasks
//       memory/
//         2026-03-19.md    — daily journal
// ===========================================================================

use std::path::{Path, PathBuf};

/// Run `dyson init`.
pub fn run(
    noinput: bool,
    daemonize: bool,
    import_filesystem: Option<PathBuf>,
    path: Option<PathBuf>,
    env_vars: Vec<String>,
    dangerous_no_sandbox: bool,
) -> dyson::error::Result<()> {
    let home = std::env::var("HOME").map_err(|_| {
        dyson::error::DysonError::Config("HOME environment variable not set".into())
    })?;
    let base = path.unwrap_or_else(|| PathBuf::from(&home).join(".dyson"));

    if !noinput {
        return Err(dyson::error::DysonError::Config(
            "interactive init not yet implemented.  Use --noinput for defaults.\n\
             Usage: dyson init --noinput"
                .into(),
        ));
    }

    eprintln!("initializing {}...", base.display());

    // Create directory structure.
    let workspace_dir = base.join("workspace");
    std::fs::create_dir_all(&workspace_dir)?;
    std::fs::create_dir_all(workspace_dir.join("memory"))?;

    // Write default config.
    let config_path = base.join("dyson.json");
    if !config_path.exists() {
        let default_config = serde_json::json!({
            "config_version": dyson::config::migrate::CURRENT_VERSION,
            "providers": {
                "default": {
                    "type": "claude-code",
                    "model": "sonnet"
                }
            },
            "agent": {
                "provider": "default",
                "max_iterations": 80,
                "max_tokens": 16384
            },
            "workspace": {
                "path": workspace_dir.to_string_lossy()
            },
            "controllers": [
                { "type": "terminal" }
            ],
            "skills": {
                "builtin": {
                    "tools": ["bash"]
                }
            }
        });

        let json = serde_json::to_string_pretty(&default_config)?;
        std::fs::write(&config_path, format!("{json}\n"))?;
        // Restrict permissions — config may contain secrets.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o600));
        }
        eprintln!("  created {}", config_path.display());
    } else {
        eprintln!("  {} already exists — skipping", config_path.display());
    }

    // Import filesystem workspace if requested.
    if let Some(ref source) = import_filesystem {
        import_filesystem_workspace(source, &workspace_dir)?;
    } else if is_filesystem_workspace(&workspace_dir) {
        eprintln!("  detected existing filesystem workspace — migrating in place");
    }

    // Load workspace — runs migrations (v0 → current), then creates
    // default files for anything missing (USER.md, HEARTBEAT.md, etc.).
    let _ = dyson::workspace::FilesystemWorkspace::load(
        &workspace_dir,
        dyson::config::MemoryConfig::default(),
    )?;
    eprintln!("  workspace ready at {}", workspace_dir.display());

    // Install binary to PATH.
    install_to_path(&base)?;

    if daemonize {
        install_systemd_service(&base, &config_path, &env_vars, dangerous_no_sandbox)?;
    } else {
        eprintln!();
        eprintln!("done. to start:");
        eprintln!("  dyson listen --config {}", config_path.display());
        eprintln!();
        eprintln!("to install as a service:");
        eprintln!("  dyson init --noinput --daemonize");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// filesystem detection and import
// ---------------------------------------------------------------------------

/// Check if a directory looks like an existing filesystem workspace.
///
/// Returns `true` if it contains at least SOUL.md and IDENTITY.md.
/// These are the two files every filesystem/TARS workspace has.
fn is_filesystem_workspace(path: &Path) -> bool {
    path.join("SOUL.md").exists() && path.join("IDENTITY.md").exists()
}

/// Copy an filesystem workspace directory into the Dyson workspace.
///
/// Copies all .md files from the source root and the memory/ subdirectory.
/// Existing files in the destination are overwritten.
fn import_filesystem_workspace(source: &Path, dest: &Path) -> dyson::error::Result<()> {
    if !source.exists() {
        return Err(dyson::error::DysonError::Config(format!(
            "filesystem workspace not found: {}",
            source.display()
        )));
    }

    eprintln!(
        "  importing filesystem workspace from {}...",
        source.display()
    );

    let mut count = 0;

    // Copy top-level .md files.
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".md") {
                std::fs::copy(entry.path(), dest.join(&name))?;
                count += 1;
            }
        }
    }

    // Copy memory/ directory.
    let source_memory = source.join("memory");
    if source_memory.exists() {
        let dest_memory = dest.join("memory");
        std::fs::create_dir_all(&dest_memory)?;
        for entry in std::fs::read_dir(&source_memory)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.ends_with(".md") {
                    std::fs::copy(entry.path(), dest_memory.join(&name))?;
                    count += 1;
                }
            }
        }
    }

    eprintln!("  imported {count} files");
    Ok(())
}

// ---------------------------------------------------------------------------
// Binary installation
// ---------------------------------------------------------------------------

/// Copy the dyson binary into ~/.dyson/bin/ and symlink to ~/.local/bin/
/// so it's on PATH without modifying shell configs.
///
/// ~/.local/bin/ is on PATH by default on most Linux distros (via
/// systemd's user environment) and on macOS if the user has it configured.
/// If ~/.local/bin/ doesn't exist, we create it — the user may need to
/// add it to PATH manually (we print instructions).
fn install_to_path(base: &Path) -> dyson::error::Result<()> {
    let current_exe = std::env::current_exe()?;

    // Copy binary into ~/.dyson/bin/.
    let bin_dir = base.join("bin");
    std::fs::create_dir_all(&bin_dir)?;
    let installed_bin = bin_dir.join("dyson");
    std::fs::copy(&current_exe, &installed_bin)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&installed_bin, std::fs::Permissions::from_mode(0o755))?;
    }

    eprintln!("  installed binary to {}", installed_bin.display());

    // Symlink into ~/.local/bin/ so it's on PATH.
    let home = std::env::var("HOME").unwrap_or_default();
    let local_bin = PathBuf::from(&home).join(".local/bin");
    std::fs::create_dir_all(&local_bin)?;

    let symlink_path = local_bin.join("dyson");

    // Remove existing symlink/file if present.  We skip the existence check
    // to avoid a TOCTOU race — just attempt removal and ignore NotFound.
    match std::fs::remove_file(&symlink_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }

    #[cfg(unix)]
    std::os::unix::fs::symlink(&installed_bin, &symlink_path)?;

    eprintln!(
        "  symlinked {} -> {}",
        symlink_path.display(),
        installed_bin.display()
    );

    // Check if ~/.local/bin is actually on PATH.
    let path_var = std::env::var("PATH").unwrap_or_default();
    if !path_var.split(':').any(|p| Path::new(p) == local_bin) {
        eprintln!();
        eprintln!("  note: {} is not on your PATH.", local_bin.display());
        eprintln!("  add this to your shell config (~/.bashrc or ~/.zshrc):");
        eprintln!("    export PATH=\"$HOME/.local/bin:$PATH\"");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Systemd service
// ---------------------------------------------------------------------------

/// Install a systemd user service for Dyson.
///
/// Creates ~/.config/systemd/user/dyson.service and enables it.
/// Falls back to /etc/systemd/system/dyson.service with sudo if
/// user services aren't available.
///
/// Intentionally synchronous: `dyson init` is a one-shot CLI command with no
/// concurrent async work, so blocking `std::process::Command` calls are fine.
fn install_systemd_service(
    base: &Path,
    config_path: &Path,
    env_vars: &[String],
    dangerous_no_sandbox: bool,
) -> dyson::error::Result<()> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (base, config_path, env_vars, dangerous_no_sandbox);
        eprintln!("--daemonize is only supported on Linux (systemd).");
        eprintln!("on macOS, use launchd instead (not yet implemented).");
        Ok(())
    }

    #[cfg(target_os = "linux")]
    {
        // Use the installed copy at ~/.dyson/bin/dyson (created by install_to_path)
        // rather than the current exe, so the service survives the original
        // binary being moved or deleted.
        let dyson_bin = base.join("bin").join("dyson");

        let user = std::env::var("USER").unwrap_or_else(|_| "root".into());
        let home = std::env::var("HOME").unwrap_or_default();
        let path = std::env::var("PATH").unwrap_or_default();

        // Build extra CLI flags for ExecStart.
        let extra_listen_args = if dangerous_no_sandbox {
            " --dangerous-no-sandbox"
        } else {
            ""
        };

        // Build extra Environment= lines from --env flags.
        let extra_env: String = env_vars
            .iter()
            .map(|kv| format!("Environment={kv}\n"))
            .collect();

        // Build the service unit file.
        //
        // We capture the current PATH so that binaries like `claude` (installed
        // via npm into ~/.local/bin or ~/.nvm/...) are found in the service
        // environment.  Without this, systemd's minimal PATH won't include
        // npm global directories and claude-code provider will fail to spawn.
        //
        // The user and system units differ only by the `User=` line and the
        // `WantedBy=` target, so one builder covers both.
        let build_unit = |user_line: &str, wanted_by: &str| {
            format!(
                "[Unit]\n\
                 Description=Dyson AI Agent\n\
                 After=network.target\n\
                 \n\
                 [Service]\n\
                 Type=simple\n\
                 {user_line}ExecStart={dyson_bin} listen --config {config_path}{extra_listen_args}\n\
                 Restart=on-failure\n\
                 RestartSec=5\n\
                 WorkingDirectory={home}\n\
                 Environment=HOME={home}\n\
                 Environment=PATH={path}\n\
                 {extra_env}\
                 \n\
                 [Install]\n\
                 WantedBy={wanted_by}\n",
                dyson_bin = dyson_bin.display(),
                config_path = config_path.display(),
            )
        };

        let unit = build_unit("", "default.target");

        // Try user service first (no sudo needed).
        let user_service_dir = PathBuf::from(&home).join(".config/systemd/user");
        let user_service_path = user_service_dir.join("dyson.service");

        eprintln!("installing systemd service...");

        if std::fs::create_dir_all(&user_service_dir).is_ok() {
            std::fs::write(&user_service_path, &unit)?;
            eprintln!("  created {}", user_service_path.display());

            // Enable and start — warn on failure rather than silently ignoring.
            let systemd_ok = run_systemctl("systemctl", &["--user"]);

            if systemd_ok {
                eprintln!("  enabled and started (user service)");
            } else {
                eprintln!("  service created but systemctl commands had errors (see above)");
            }
            eprintln!();
            eprintln!("manage with:");
            eprintln!("  systemctl --user status dyson");
            eprintln!("  systemctl --user restart dyson");
            eprintln!("  journalctl --user -u dyson -f");
        } else {
            // Fall back to system service with sudo.
            eprintln!("  user service dir not available, using system service (needs sudo)");

            let system_path = PathBuf::from("/etc/systemd/system/dyson.service");
            let unit_system = build_unit(&format!("User={user}\n"), "multi-user.target");

            // Write via sudo tee.
            let mut child = std::process::Command::new("sudo")
                .args(["tee", &system_path.to_string_lossy()])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .spawn()?;

            if let Some(mut stdin) = child.stdin.take() {
                use std::io::Write;
                stdin.write_all(unit_system.as_bytes())?;
            }
            child.wait()?;

            eprintln!("  created {}", system_path.display());

            let systemd_ok = run_systemctl("sudo", &["systemctl"]);

            if systemd_ok {
                eprintln!("  enabled and started (system service)");
            } else {
                eprintln!("  service created but systemctl commands had errors (see above)");
            }
            eprintln!();
            eprintln!("manage with:");
            eprintln!("  sudo systemctl status dyson");
            eprintln!("  sudo systemctl restart dyson");
            eprintln!("  sudo journalctl -u dyson -f");
        }

        Ok(())
    }
}

/// Run the `daemon-reload` → `enable dyson` → `start dyson` systemctl steps,
/// prefixing each invocation with `program` + `prefix_args` (e.g.
/// `systemctl --user …` for the user service, `sudo systemctl …` for the
/// system service).  Warns on failure rather than aborting; returns whether
/// every step succeeded.
#[cfg(target_os = "linux")]
fn run_systemctl(program: &str, prefix_args: &[&str]) -> bool {
    let mut ok = true;
    for step in [
        &["daemon-reload"][..],
        &["enable", "dyson"],
        &["start", "dyson"],
    ] {
        let args: Vec<&str> = prefix_args.iter().chain(step.iter()).copied().collect();
        match std::process::Command::new(program).args(&args).status() {
            Ok(s) if s.success() => {}
            Ok(s) => {
                eprintln!("  warning: {program} {} exited with {s}", args.join(" "));
                ok = false;
            }
            Err(e) => {
                eprintln!("  warning: failed to run {program} {}: {e}", args.join(" "));
                ok = false;
            }
        }
    }
    ok
}
