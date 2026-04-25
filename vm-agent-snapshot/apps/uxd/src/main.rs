use agent_common::{AgentConfig, init_tracing};
use anyhow::Result;
use clap::Parser;
use tracing::info;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "config/agent.example.toml")]
    config: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();
    let _cfg = AgentConfig::load(&args.config)?;

    #[cfg(target_os = "linux")]
    {
        let mut guard = linux::UxGuard::apply().await?;
        info!("ux optimizations applied; waiting for shutdown");
        guard.wait_for_shutdown().await?;
        guard.restore().await?;
    }

    #[cfg(not(target_os = "linux"))]
    {
        info!("uxd is a no-op outside Linux");
        tokio::signal::ctrl_c().await?;
    }

    Ok(())
}

#[cfg(target_os = "linux")]
mod linux {
    use anyhow::{Context, Result};
    use std::{
        env, fs,
        path::{Path, PathBuf},
        process::Stdio,
    };
    use tokio::{process::Command, signal};

    pub struct UxGuard {
        file_backups: Vec<FileBackup>,
        gsettings_backup: Option<String>,
    }

    struct FileBackup {
        path: PathBuf,
        original: Option<String>,
    }

    impl UxGuard {
        pub async fn apply() -> Result<Self> {
            let mut guard = Self {
                file_backups: Vec::new(),
                gsettings_backup: read_gsettings().await.ok(),
            };

            for path in gtk_settings_paths()? {
                guard.patch_gtk_settings(path)?;
            }
            let _ = run_quiet(
                "gsettings",
                &[
                    "set",
                    "org.gnome.desktop.interface",
                    "enable-animations",
                    "false",
                ],
            )
            .await;
            let _ = run_quiet(
                "xprop",
                &[
                    "-root",
                    "-f",
                    "_GTK_ENABLE_ANIMATIONS",
                    "32c",
                    "-set",
                    "_GTK_ENABLE_ANIMATIONS",
                    "0",
                ],
            )
            .await;
            write_runtime_env()?;
            write_reduced_motion_css()?;

            Ok(guard)
        }

        pub async fn wait_for_shutdown(&mut self) -> Result<()> {
            #[cfg(unix)]
            {
                let mut term = signal::unix::signal(signal::unix::SignalKind::terminate())?;
                tokio::select! {
                    _ = signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            #[cfg(not(unix))]
            signal::ctrl_c().await?;
            Ok(())
        }

        pub async fn restore(&mut self) -> Result<()> {
            for backup in self.file_backups.drain(..).rev() {
                match backup.original {
                    Some(contents) => fs::write(&backup.path, contents)
                        .with_context(|| format!("restore {}", backup.path.display()))?,
                    None => {
                        let _ = fs::remove_file(&backup.path);
                    }
                }
            }
            if let Some(value) = self.gsettings_backup.take() {
                let _ = run_quiet(
                    "gsettings",
                    &[
                        "set",
                        "org.gnome.desktop.interface",
                        "enable-animations",
                        value.trim(),
                    ],
                )
                .await;
            }
            let _ = run_quiet("xprop", &["-root", "-remove", "_GTK_ENABLE_ANIMATIONS"]).await;
            Ok(())
        }

        fn patch_gtk_settings(&mut self, path: PathBuf) -> Result<()> {
            let original = fs::read_to_string(&path).ok();
            let patched = patch_gtk_enable_animations(original.as_deref().unwrap_or_default());
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&path, patched)?;
            self.file_backups.push(FileBackup { path, original });
            Ok(())
        }
    }

    impl Drop for UxGuard {
        fn drop(&mut self) {
            for backup in self.file_backups.drain(..).rev() {
                match backup.original {
                    Some(contents) => {
                        let _ = fs::write(&backup.path, contents);
                    }
                    None => {
                        let _ = fs::remove_file(&backup.path);
                    }
                }
            }
        }
    }

    fn gtk_settings_paths() -> Result<Vec<PathBuf>> {
        let home = env::var("HOME").context("HOME is not set")?;
        Ok(vec![
            Path::new(&home).join(".config/gtk-3.0/settings.ini"),
            Path::new(&home).join(".config/gtk-4.0/settings.ini"),
        ])
    }

    fn patch_gtk_enable_animations(input: &str) -> String {
        let mut output = String::new();
        let mut saw_settings = false;
        let mut in_settings = false;
        let mut wrote_key = false;

        for line in input.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                if in_settings && !wrote_key {
                    output.push_str("gtk-enable-animations=false\n");
                    wrote_key = true;
                }
                in_settings = trimmed == "[Settings]";
                saw_settings |= in_settings;
                output.push_str(line);
                output.push('\n');
                continue;
            }

            if in_settings && trimmed.starts_with("gtk-enable-animations") {
                output.push_str("gtk-enable-animations=false\n");
                wrote_key = true;
            } else {
                output.push_str(line);
                output.push('\n');
            }
        }

        if !saw_settings {
            output.push_str("[Settings]\n");
            output.push_str("gtk-enable-animations=false\n");
        } else if in_settings && !wrote_key {
            output.push_str("gtk-enable-animations=false\n");
        }
        output
    }

    async fn read_gsettings() -> Result<String> {
        let output = Command::new("gsettings")
            .args(["get", "org.gnome.desktop.interface", "enable-animations"])
            .output()
            .await?;
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    async fn run_quiet(program: &str, args: &[&str]) -> Result<()> {
        let _ = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await?;
        Ok(())
    }

    fn write_runtime_env() -> Result<()> {
        fs::create_dir_all("/tmp/agent")?;
        fs::write(
            "/tmp/agent/agent-env.sh",
            "export GTK_ENABLE_ANIMATIONS=0\nexport QT_STYLE_OVERRIDE=Fusion\nexport QT_QPA_PLATFORMTHEME=gtk3\nexport QT_QPA_PLATFORM_THEME=gtk3\n",
        )?;
        Ok(())
    }

    fn write_reduced_motion_css() -> Result<()> {
        fs::create_dir_all("/tmp/agent")?;
        fs::write(
            "/tmp/agent/reduced-motion.css",
            "*{scroll-behavior:auto!important;transition-duration:0.001ms!important;animation-duration:0.001ms!important;animation-iteration-count:1!important;}\n",
        )?;
        Ok(())
    }
}
