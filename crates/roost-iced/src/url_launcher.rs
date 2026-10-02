use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Platform {
    MacOs,
    Linux,
    Unsupported,
}

impl Platform {
    fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else {
            Self::Unsupported
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LaunchCommand {
    program: &'static str,
    args: Vec<String>,
}

/// What an action asks the OS to open. Both kinds go through
/// [`plan`], so test mode has one place to refuse to launch anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum External {
    Url(String),
    /// A file for the default text editor; `seed` is written first when
    /// the file does not exist yet.
    File {
        path: PathBuf,
        seed: Option<&'static str>,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ExternalPlan {
    /// Test mode: say what would have opened and launch nothing.
    WouldOpen(String),
    Launch(External),
}

pub(crate) fn plan(test_mode: bool, what: External) -> ExternalPlan {
    if !test_mode {
        return ExternalPlan::Launch(what);
    }
    ExternalPlan::WouldOpen(match what {
        External::Url(url) => url,
        External::File { path, .. } => path.display().to_string(),
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LauncherExit {
    success: bool,
    description: String,
}

fn command_for(platform: Platform, url: String) -> Result<LaunchCommand, String> {
    validate_uri(&url)?;
    let program = match platform {
        Platform::MacOs => "/usr/bin/open",
        Platform::Linux => "xdg-open",
        Platform::Unsupported => return Err("URL opening is unsupported on this platform".into()),
    };
    Ok(LaunchCommand {
        program,
        args: vec![url],
    })
}

fn command_for_file(platform: Platform, path: &Path) -> Result<LaunchCommand, String> {
    let path = path
        .to_str()
        .filter(|path| path.starts_with('/') && !path.chars().any(char::is_control))
        .ok_or_else(|| "file launcher requires an absolute path".to_string())?
        .to_string();
    let (program, mut args) = match platform {
        Platform::MacOs => ("/usr/bin/open", vec!["-t".to_string()]),
        Platform::Linux => ("xdg-open", Vec::new()),
        Platform::Unsupported => return Err("file opening is unsupported on this platform".into()),
    };
    args.push(path);
    Ok(LaunchCommand { program, args })
}

/// Create `path` (and its parent directory) holding `seed` when it does
/// not exist. An existing file is never touched. Returns whether it was
/// created.
pub(crate) fn create_if_missing(path: &Path, seed: &str) -> std::io::Result<bool> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => {
            writeln!(file, "{seed}")?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error),
    }
}

fn validate_uri(uri: &str) -> Result<(), String> {
    let Some((scheme, _)) = uri.split_once(':') else {
        return Err("URL launcher requires an absolute URI".into());
    };
    let mut chars = scheme.chars();
    if !chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        || !chars.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')
        })
        || uri.chars().any(char::is_control)
    {
        return Err("URL launcher rejected an invalid URI".into());
    }
    Ok(())
}

async fn open_with<R, F>(platform: Platform, url: String, runner: R) -> Result<(), String>
where
    R: FnOnce(LaunchCommand) -> F,
    F: Future<Output = Result<LauncherExit, String>>,
{
    run_command(command_for(platform, url)?, runner).await
}

async fn run_command<R, F>(command: LaunchCommand, runner: R) -> Result<(), String>
where
    R: FnOnce(LaunchCommand) -> F,
    F: Future<Output = Result<LauncherExit, String>>,
{
    let program = command.program;
    let exit = runner(command)
        .await
        .map_err(|error| format!("URL launcher {program} could not start: {error}"))?;
    if exit.success {
        Ok(())
    } else {
        Err(format!(
            "URL launcher {program} exited unsuccessfully: {}",
            exit.description
        ))
    }
}

async fn spawn(command: LaunchCommand) -> Result<LauncherExit, String> {
    let status = tokio::process::Command::new(command.program)
        .args(&command.args)
        .status()
        .await
        .map_err(|error| error.to_string())?;
    Ok(LauncherExit {
        success: status.success(),
        description: status.to_string(),
    })
}

pub(crate) async fn open(url: String) -> Result<(), String> {
    open_with(Platform::current(), url, spawn).await
}

/// Seed the file when asked, then open it in the default text editor.
/// The seed is a blocking write, so it runs on the blocking pool.
pub(crate) async fn open_file(path: PathBuf, seed: Option<&'static str>) -> Result<(), String> {
    let path = std::path::absolute(&path)
        .map_err(|error| format!("cannot resolve {}: {error}", path.display()))?;
    if let Some(seed) = seed {
        let target = path.clone();
        tokio::task::spawn_blocking(move || create_if_missing(&target, seed))
            .await
            .map_err(|error| format!("settings file creation did not join: {error}"))?
            .map_err(|error| format!("cannot create {}: {error}", path.display()))?;
    }
    run_command(command_for_file(Platform::current(), &path)?, spawn).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builders_are_argument_safe_and_cross_platform() {
        let url = "https://example.test/a path;echo-no".to_string();
        assert_eq!(
            command_for(Platform::MacOs, url.clone()).unwrap(),
            LaunchCommand {
                program: "/usr/bin/open",
                args: vec![url.clone()],
            }
        );
        assert_eq!(
            command_for(Platform::Linux, url.clone()).unwrap(),
            LaunchCommand {
                program: "xdg-open",
                args: vec![url],
            }
        );
        assert_eq!(
            command_for(Platform::Unsupported, "https://x.test".into()),
            Err("URL opening is unsupported on this platform".into())
        );
        assert_eq!(
            command_for(Platform::Linux, "--help".into()),
            Err("URL launcher requires an absolute URI".into())
        );
        assert_eq!(
            command_for(Platform::Linux, "https://x.test\n--help".into()),
            Err("URL launcher rejected an invalid URI".into())
        );
    }

    #[tokio::test]
    async fn fake_runner_maps_success_spawn_error_and_exit_error() {
        let success = open_with(Platform::Linux, "https://x.test".into(), |_| async {
            Ok(LauncherExit {
                success: true,
                description: "exit status: 0".into(),
            })
        })
        .await;
        assert_eq!(success, Ok(()));

        let spawn = open_with(Platform::Linux, "https://x.test".into(), |_| async {
            Err("not found".into())
        })
        .await;
        assert_eq!(
            spawn,
            Err("URL launcher xdg-open could not start: not found".into())
        );

        let exit = open_with(Platform::MacOs, "https://x.test".into(), |_| async {
            Ok(LauncherExit {
                success: false,
                description: "exit status: 3".into(),
            })
        })
        .await;
        assert_eq!(
            exit,
            Err("URL launcher /usr/bin/open exited unsuccessfully: exit status: 3".into())
        );
    }
}

#[cfg(test)]
mod external_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn counting_exit(calls: &AtomicUsize) -> LauncherExit {
        calls.fetch_add(1, Ordering::SeqCst);
        LauncherExit {
            success: true,
            description: "exit status: 0".into(),
        }
    }

    /// Run what the UI would run for `what`, through a runner that counts
    /// instead of spawning, so a launch can never reach a real browser or
    /// editor.
    async fn launches(test_mode: bool, what: External) -> usize {
        let calls = AtomicUsize::new(0);
        match plan(test_mode, what) {
            ExternalPlan::WouldOpen(_) => {}
            ExternalPlan::Launch(External::Url(url)) => {
                open_with(Platform::Linux, url, |_| async {
                    Ok(counting_exit(&calls))
                })
                .await
                .unwrap();
            }
            ExternalPlan::Launch(External::File { path, .. }) => {
                let command = command_for_file(Platform::Linux, &path).unwrap();
                run_command(command, |_| async { Ok(counting_exit(&calls)) })
                    .await
                    .unwrap();
            }
        }
        calls.load(Ordering::SeqCst)
    }

    fn file() -> External {
        External::File {
            path: PathBuf::from("/tmp/roost-test/config.conf"),
            seed: None,
        }
    }

    #[tokio::test]
    async fn test_mode_never_reaches_the_launcher() {
        assert_eq!(
            launches(true, External::Url("https://x.test/".into())).await,
            0
        );
        assert_eq!(launches(true, file()).await, 0);
    }

    /// Preservation: outside test mode the same calls do launch, so the
    /// zero above is the gate and not a runner that never fires.
    #[tokio::test]
    async fn outside_test_mode_each_open_launches_once() {
        assert_eq!(
            launches(false, External::Url("https://x.test/".into())).await,
            1
        );
        assert_eq!(launches(false, file()).await, 1);
    }

    #[test]
    fn test_mode_reports_the_target_it_would_have_opened() {
        assert_eq!(
            plan(true, External::Url("https://x.test/".into())),
            ExternalPlan::WouldOpen("https://x.test/".into())
        );
        assert_eq!(
            plan(true, file()),
            ExternalPlan::WouldOpen("/tmp/roost-test/config.conf".into())
        );
    }

    #[test]
    fn file_commands_open_the_text_editor_per_platform() {
        let path = Path::new("/home/u/.config/roost/config.conf");
        assert_eq!(
            command_for_file(Platform::MacOs, path).unwrap(),
            LaunchCommand {
                program: "/usr/bin/open",
                args: vec!["-t".into(), path.display().to_string()],
            }
        );
        assert_eq!(
            command_for_file(Platform::Linux, path).unwrap(),
            LaunchCommand {
                program: "xdg-open",
                args: vec![path.display().to_string()],
            }
        );
        assert!(command_for_file(Platform::Linux, Path::new("-t")).is_err());
        assert!(command_for_file(Platform::Linux, Path::new("/a\u{7}b")).is_err());
    }

    #[test]
    fn the_seed_is_written_once_and_an_existing_file_is_untouched() {
        let dir = std::env::temp_dir().join(format!("r073-c2-seed-{}", std::process::id()));
        let path = dir.join("nested/config.conf");
        assert!(create_if_missing(&path, "# header").unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# header\n");
        std::fs::write(&path, "theme = x\n").unwrap();
        assert!(!create_if_missing(&path, "# header").unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "theme = x\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
