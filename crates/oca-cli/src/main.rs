use std::process::ExitCode;

fn main() -> ExitCode {
    let arguments = std::env::args().collect::<Vec<_>>();
    if option_before_end(&arguments, "--help") || option_before_end(&arguments, "-h") {
        println!("{}", oca_cli::help_text());
        return ExitCode::SUCCESS;
    }
    let json = option_before_end(&arguments, "--json");

    match oca_cli::parse_from(arguments) {
        Ok(_) => ExitCode::SUCCESS,
        Err(error) => {
            if json {
                eprintln!("{}", error.to_json());
            } else {
                eprint!("{}", error.render_failure());
            }
            ExitCode::from(u8::try_from(error.exit_code()).unwrap_or(1))
        }
    }
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
            "opus:h".to_owned(),
            "--".to_owned(),
            "--help".to_owned(),
            "--json".to_owned(),
        ];

        assert!(!option_before_end(&arguments, "--help"));
        assert!(!option_before_end(&arguments, "--json"));
    }
}
