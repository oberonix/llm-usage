use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

const DESKTOP_FILE_NAME: &str = "llm-usage.desktop";
#[cfg(target_os = "macos")]
const LAUNCH_AGENT_FILE_NAME: &str = "dev.buffbit.llm-usage.plist";

pub fn set_start_at_login(enabled: bool) -> Result<()> {
    let tray = resolve_tray_binary()?;
    set_start_at_login_for(enabled, &tray)
}

pub fn is_start_at_login_enabled() -> bool {
    autostart_path().is_some_and(|p| p.exists())
}

fn set_start_at_login_for(enabled: bool, tray_binary: &Path) -> Result<()> {
    let path = autostart_path().context("login items are not supported on this platform")?;
    if enabled {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        std::fs::write(&path, autostart_file_contents(tray_binary))
            .with_context(|| format!("write {}", path.display()))?;
    } else if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
    }
    Ok(())
}

fn autostart_path() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        return dirs::config_dir().map(|d| d.join("autostart").join(DESKTOP_FILE_NAME));
    }
    #[cfg(target_os = "macos")]
    {
        return dirs::home_dir().map(|h| {
            h.join("Library")
                .join("LaunchAgents")
                .join(LAUNCH_AGENT_FILE_NAME)
        });
    }
    #[allow(unreachable_code)]
    None
}

fn resolve_tray_binary() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("resolve current executable")?;
    Ok(sibling_tray_binary(&exe))
}

fn sibling_tray_binary(current_exe: &Path) -> PathBuf {
    let Some(parent) = current_exe.parent() else {
        return current_exe.to_path_buf();
    };
    let name = if cfg!(windows) {
        "llm-usage-tray.exe"
    } else {
        "llm-usage-tray"
    };
    parent.join(name)
}

fn autostart_file_contents(tray_binary: &Path) -> String {
    #[cfg(target_os = "linux")]
    {
        return format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=LLM Usage\n\
             Comment=Menu bar widget for LLM account usage and spend\n\
             Exec={}\n\
             Icon=llm-usage\n\
             Terminal=false\n\
             Categories=Utility;Development;\n\
             StartupNotify=false\n\
             X-GNOME-Autostart-enabled=true\n",
            tray_binary.display()
        );
    }
    #[cfg(target_os = "macos")]
    {
        let escaped = xml_escape(&tray_binary.display().to_string());
        return format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
             \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n\
             <dict>\n\
                 <key>Label</key>\n\
                 <string>dev.buffbit.llm-usage</string>\n\
                 <key>ProgramArguments</key>\n\
                 <array>\n\
                     <string>{}</string>\n\
                 </array>\n\
                 <key>RunAtLoad</key>\n\
                 <true/>\n\
                 <key>KeepAlive</key>\n\
                 <dict>\n\
                     <key>SuccessfulExit</key>\n\
                     <false/>\n\
                 </dict>\n\
                 <key>StandardOutPath</key>\n\
                 <string>/tmp/llm-usage.out.log</string>\n\
                 <key>StandardErrorPath</key>\n\
                 <string>/tmp/llm-usage.err.log</string>\n\
             </dict>\n\
             </plist>\n",
            escaped
        );
    }
    #[allow(unreachable_code)]
    String::new()
}

#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sibling_tray_binary_replaces_dashboard_name() {
        let path = Path::new("/tmp/build/llm-usage-dashboard");
        assert_eq!(
            sibling_tray_binary(path),
            PathBuf::from("/tmp/build/llm-usage-tray")
        );
    }

    #[test]
    fn linux_desktop_file_points_at_tray_binary() {
        let body = autostart_file_contents(Path::new("/opt/llm-usage/llm-usage-tray"));
        if cfg!(target_os = "linux") {
            assert!(body.contains("Exec=/opt/llm-usage/llm-usage-tray"));
            assert!(body.contains("X-GNOME-Autostart-enabled=true"));
        }
    }
}
