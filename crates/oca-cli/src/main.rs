use std::{future::Future, process::ExitCode};

fn main() -> ExitCode {
    let arguments = std::env::args().collect::<Vec<_>>();
    if option_before_end(&arguments, "--help") || option_before_end(&arguments, "-h") {
        println!("{}", oca_cli::help_text());
        return ExitCode::SUCCESS;
    }
    let json = option_before_end(&arguments, "--json");

    let result = home_directory()
        .ok_or_else(|| {
            oca_core::OcaError::new(oca_core::ErrorCode::Usage)
                .with_error("could not determine the home directory")
                .with_help("set the HOME environment variable and retry")
        })
        .and_then(|home| oca_cli::parse_from_home(arguments, &home).map(|command| (home, command)));

    match result {
        Ok((home, oca_cli::Command::Dispatch(command))) => {
            let background = command.background;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| {
                    oca_core::OcaError::new(oca_core::ErrorCode::ServerUnavailable)
                        .with_error(format!("could not start async runtime: {error}"))
                });
            match runtime {
                Ok(runtime) => {
                    let execution = if background {
                        runtime.block_on(oca_cli::execute_background(command, home))
                    } else {
                        runtime.block_on(oca_cli::execute_foreground(command, home))
                    };
                    match execution {
                        Ok(()) => ExitCode::SUCCESS,
                        Err(error) => render_error(&error, json),
                    }
                }
                Err(error) => render_error(&error, json),
            }
        }
        Ok((home, oca_cli::Command::Follow(command))) => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("the current-thread runtime must initialize");
            match runtime.block_on(oca_cli::execute_follow(&command, home)) {
                Ok(output) => {
                    print!("{}", output.stdout);
                    ExitCode::from(u8::try_from(output.exit.code()).unwrap_or(1))
                }
                Err(error) => render_error(&error, json),
            }
        }
        Ok((home, oca_cli::Command::List(command))) => {
            match oca_cli::execute_list(&command, home) {
                Ok(output) => {
                    print!("{output}");
                    ExitCode::SUCCESS
                }
                Err(error) => render_error(&error, json),
            }
        }
        Ok((home, oca_cli::Command::Events(command))) => {
            match oca_cli::execute_events(&command, home) {
                Ok(output) => {
                    print!("{output}");
                    ExitCode::SUCCESS
                }
                Err(error) => render_error(&error, json),
            }
        }
        Ok((home, oca_cli::Command::Message(command))) => {
            run_control(oca_cli::execute_message(&command, home), json)
        }
        Ok((home, oca_cli::Command::Queue(command))) => {
            run_control(oca_cli::execute_queue(&command, home), json)
        }
        Ok((home, oca_cli::Command::Abort(command))) => {
            run_control(oca_cli::execute_abort(&command, home), json)
        }
        Ok(_) => ExitCode::SUCCESS,
        Err(error) => render_error(&error, json),
    }
}

fn run_control(
    operation: impl Future<Output = Result<oca_cli::ControlCommandOutput, oca_core::OcaError>>,
    json: bool,
) -> ExitCode {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("the current-thread runtime must initialize");
    match runtime.block_on(operation) {
        Ok(output) => {
            print!("{}", output.stdout);
            ExitCode::SUCCESS
        }
        Err(error) => render_error(&error, json),
    }
}

fn render_error(error: &oca_core::OcaError, json: bool) -> ExitCode {
    if json {
        eprintln!("{}", error.to_json());
    } else {
        eprint!("{}", error.render_failure());
    }
    ExitCode::from(u8::try_from(error.exit_code()).unwrap_or(1))
}

fn home_directory() -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    const HOME_VARIABLE: &str = "USERPROFILE";
    #[cfg(not(windows))]
    const HOME_VARIABLE: &str = "HOME";

    std::env::var_os(HOME_VARIABLE).map(Into::into)
}

fn option_before_end(arguments: &[String], option: &str) -> bool {
    arguments
        .iter()
        .skip(1)
        .take_while(|argument| argument.as_str() != "--")
        .any(|argument| argument == option)
}

#[cfg(test)]
mod tests {
    use super::option_before_end;

    #[test]
    fn end_of_options_keeps_help_and_json_as_literal_prompt_text() {
        let arguments = vec![
            "oca".to_owned(),
            "luna:h".to_owned(),
            "--".to_owned(),
            "--help".to_owned(),
            "--json".to_owned(),
        ];

        assert!(!option_before_end(&arguments, "--help"));
        assert!(!option_before_end(&arguments, "--json"));
    }
}
