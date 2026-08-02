use std::future::Future;

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
    argument: String,
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
        argument: url,
    })
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
    let command = command_for(platform, url)?;
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

pub(crate) async fn open(url: String) -> Result<(), String> {
    open_with(Platform::current(), url, |command| async move {
        let status = tokio::process::Command::new(command.program)
            .arg(&command.argument)
            .status()
            .await
            .map_err(|error| error.to_string())?;
        Ok(LauncherExit {
            success: status.success(),
            description: status.to_string(),
        })
    })
    .await
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
                argument: url.clone(),
            }
        );
        assert_eq!(
            command_for(Platform::Linux, url.clone()).unwrap(),
            LaunchCommand {
                program: "xdg-open",
                argument: url,
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
