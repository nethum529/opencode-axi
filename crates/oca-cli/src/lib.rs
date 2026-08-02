use std::path::PathBuf;

use oca_core::{EffortInput, ErrorCode, ModelCatalog, OcaError, ResolvedModel, resolve_model};

/// The fixed command grammar accepted by `oca`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    Dispatch(DispatchCommand),
    Message(MessageCommand),
    Queue(MessageCommand),
    Follow(FollowCommand),
    Abort(AbortCommand),
    List(ListCommand),
    Events(EventsCommand),
    Push(RefCommand),
    PullRequest(RefCommand),
    Attach(AttachCommand),
}

const PUBLIC_COMMANDS: [&str; 8] = ["m", "q", "f", "k", "ls", "events", "push", "pr"];

/// The commands published to help, generated schemas, and the agent skill.
#[must_use]
pub const fn public_commands() -> &'static [&'static str] {
    &PUBLIC_COMMANDS
}

/// The concise agent-facing command reference.
#[must_use]
pub const fn help_text() -> &'static str {
    "Usage: oca [--json] <alias>:<effort> [options] [--] <prompt...>\n\
     \nCommands: m q f k ls events push pr\n\
     \nModel options: -e, --role, -w, --headless, --json"
}

/// A model turn to dispatch to `OpenCode`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DispatchCommand {
    pub model: ResolvedModel,
    pub prompt: String,
    pub role: String,
    pub worktree: bool,
    pub headless: bool,
    pub json: bool,
}

/// A ref-scoped message operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessageCommand {
    pub reference: String,
    pub message: String,
    pub effort: Option<String>,
    pub json: bool,
}

/// Options for `oca f`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FollowCommand {
    pub reference: String,
    pub json: bool,
}

/// Options for `oca k`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AbortCommand {
    pub reference: String,
    pub json: bool,
}

/// Options for `oca ls`.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ListCommand {
    pub all: bool,
    pub blocked: bool,
    pub count: bool,
    pub json: bool,
}

/// Options for `oca events`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventsCommand {
    pub reference: String,
    pub since: Option<u64>,
    pub json: bool,
}

/// Options common to ref-only commands.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefCommand {
    pub reference: String,
    pub json: bool,
}

/// The detached display helper. It is deliberately excluded from agent-facing
/// grammar metadata and help text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttachCommand {
    pub reference: String,
    pub session_id: String,
    pub cwd: PathBuf,
}

/// Parse an argv sequence, including its executable name.
///
/// # Errors
///
/// Returns a T07 usage envelope for an invalid command spelling.
pub fn parse_from<I, S>(arguments: I) -> Result<Command, OcaError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut arguments = arguments.into_iter().map(Into::into);
    let _program_name = arguments.next();
    parse_arguments(arguments.collect())
}

fn parse_arguments(mut arguments: Vec<String>) -> Result<Command, OcaError> {
    let mut global_json = false;
    while arguments
        .first()
        .is_some_and(|argument| argument == "--json")
    {
        global_json = true;
        arguments.remove(0);
    }
    let Some(verb) = arguments.first() else {
        return Err(usage("a command is required"));
    };

    let tail = &arguments[1..];
    // These exact tokens are control verbs, never model aliases.
    let command = match verb.as_str() {
        "m" => parse_message(tail, true).map(Command::Message),
        "q" => parse_message(tail, false).map(Command::Queue),
        "f" => parse_follow(tail).map(Command::Follow),
        "k" => parse_abort(tail).map(Command::Abort),
        "ls" => parse_list(tail).map(Command::List),
        "events" => parse_events(tail).map(Command::Events),
        "push" => parse_ref_command(tail).map(Command::Push),
        "pr" => parse_ref_command(tail).map(Command::PullRequest),
        "__attach" => parse_attach(tail).map(Command::Attach),
        "s" => Err(usage("`s` is not a supported command")),
        _ => parse_dispatch(verb, tail).map(Command::Dispatch),
    };
    command.map(|mut command| {
        if global_json {
            command.with_json();
        }
        command
    })
}

impl Command {
    fn with_json(&mut self) {
        match self {
            Self::Dispatch(command) => command.json = true,
            Self::Message(command) | Self::Queue(command) => command.json = true,
            Self::Follow(command) => command.json = true,
            Self::Abort(command) => command.json = true,
            Self::List(command) => command.json = true,
            Self::Events(command) => command.json = true,
            Self::Push(command) | Self::PullRequest(command) => command.json = true,
            Self::Attach(_) => {}
        }
    }
}

fn parse_dispatch(verb: &str, arguments: &[String]) -> Result<DispatchCommand, OcaError> {
    let (alias, inline_effort) = match verb.split_once(':') {
        Some((alias, effort)) => (alias, Some(effort.to_owned())),
        None => (verb, None),
    };
    let mut flagged_effort = None;
    let mut role = "impl".to_owned();
    let mut worktree = false;
    let mut headless = false;
    let mut json = false;
    let mut prompt = Vec::new();
    let mut end_of_options = false;
    let mut index = 0;

    while index < arguments.len() {
        let argument = &arguments[index];
        if end_of_options {
            prompt.push(argument.clone());
        } else if argument == "--" {
            end_of_options = true;
        } else if argument == "--json" {
            json = true;
        } else if argument == "-e" || argument == "--effort" {
            index += 1;
            let Some(value) = arguments.get(index) else {
                return Err(usage("`-e` requires an effort"));
            };
            if value.starts_with('-') {
                return Err(usage("`-e` requires an effort"));
            }
            if flagged_effort.replace(value.clone()).is_some() {
                return Err(usage("effort was provided more than once"));
            }
        } else if argument == "-r" || argument == "--role" {
            index += 1;
            let Some(value) = arguments.get(index) else {
                return Err(usage("`--role` requires a role"));
            };
            if value.starts_with('-') {
                return Err(usage("`--role` requires a role"));
            }
            role.clone_from(value);
        } else if argument == "-w" || argument == "--worktree" {
            worktree = true;
        } else if argument == "--headless" {
            headless = true;
        } else if argument.starts_with('-') {
            return Err(usage(format!("unknown flag `{argument}`")));
        } else {
            prompt.push(argument.clone());
        }
        index += 1;
    }

    let model = resolve_model(
        alias,
        EffortInput::both(inline_effort, flagged_effort),
        ModelCatalog::default(),
    )?;
    if prompt.is_empty() {
        return Err(usage("a prompt is required"));
    }
    Ok(DispatchCommand {
        model,
        prompt: prompt.join(" "),
        role,
        worktree,
        headless,
        json,
    })
}

fn parse_message(arguments: &[String], permits_effort: bool) -> Result<MessageCommand, OcaError> {
    let (reference, tail) = required_first(arguments, "ref")?;
    let mut effort = None;
    let mut json = false;
    let mut message = Vec::new();
    let mut end_of_options = false;
    let mut index = 0;

    while index < tail.len() {
        let argument = &tail[index];
        if end_of_options {
            message.push(argument.clone());
        } else if argument == "--" {
            end_of_options = true;
        } else if argument == "--json" {
            json = true;
        } else if argument == "-e" || argument == "--effort" {
            if !permits_effort {
                return Err(usage("`-e` is only valid for `oca m`"));
            }
            index += 1;
            let Some(value) = tail.get(index) else {
                return Err(usage("`-e` requires an effort"));
            };
            if value.starts_with('-') {
                return Err(usage("`-e` requires an effort"));
            }
            if effort.replace(value.clone()).is_some() {
                return Err(usage("effort was provided more than once"));
            }
        } else if argument.starts_with('-') {
            return Err(usage(format!("unknown flag `{argument}`")));
        } else {
            message.push(argument.clone());
        }
        index += 1;
    }

    if message.is_empty() {
        return Err(usage("a message is required"));
    }
    Ok(MessageCommand {
        reference: reference.to_owned(),
        message: message.join(" "),
        effort,
        json,
    })
}

fn parse_follow(arguments: &[String]) -> Result<FollowCommand, OcaError> {
    let (reference, tail) = required_first(arguments, "ref")?;
    Ok(FollowCommand {
        reference: reference.to_owned(),
        json: parse_json_only(tail)?,
    })
}

fn parse_abort(arguments: &[String]) -> Result<AbortCommand, OcaError> {
    let (reference, tail) = required_first(arguments, "ref")?;
    Ok(AbortCommand {
        reference: reference.to_owned(),
        json: parse_json_only(tail)?,
    })
}

fn parse_list(arguments: &[String]) -> Result<ListCommand, OcaError> {
    let mut command = ListCommand::default();
    for argument in arguments {
        match argument.as_str() {
            "--all" => command.all = true,
            "--blocked" => command.blocked = true,
            "--count" => command.count = true,
            "--json" => command.json = true,
            _ => return Err(usage(format!("unknown flag `{argument}` for `ls`"))),
        }
    }
    Ok(command)
}

fn parse_events(arguments: &[String]) -> Result<EventsCommand, OcaError> {
    let (reference, tail) = required_first(arguments, "ref")?;
    let mut since = None;
    let mut json = false;
    let mut index = 0;
    while index < tail.len() {
        match tail[index].as_str() {
            "--json" => json = true,
            "--since" => {
                index += 1;
                let Some(value) = tail.get(index) else {
                    return Err(usage("`--since` requires a cursor"));
                };
                let cursor = value
                    .parse::<u64>()
                    .map_err(|_| usage("`--since` must be a non-negative integer"))?;
                if since.replace(cursor).is_some() {
                    return Err(usage("`--since` was provided more than once"));
                }
            }
            unknown => return Err(usage(format!("unknown flag `{unknown}` for `events`"))),
        }
        index += 1;
    }
    Ok(EventsCommand {
        reference: reference.to_owned(),
        since,
        json,
    })
}

fn parse_ref_command(arguments: &[String]) -> Result<RefCommand, OcaError> {
    let (reference, tail) = required_first(arguments, "ref")?;
    Ok(RefCommand {
        reference: reference.to_owned(),
        json: parse_json_only(tail)?,
    })
}

fn parse_attach(arguments: &[String]) -> Result<AttachCommand, OcaError> {
    let (reference, tail) = required_first(arguments, "ref")?;
    let (session_id, tail) = required_first(tail, "session id")?;
    let (cwd, tail) = required_first(tail, "cwd")?;
    if !tail.is_empty() {
        return Err(usage(
            "`__attach` accepts exactly a ref, session id, and cwd",
        ));
    }
    Ok(AttachCommand {
        reference: reference.to_owned(),
        session_id: session_id.to_owned(),
        cwd: cwd.into(),
    })
}

fn required_first<'a>(
    arguments: &'a [String],
    name: &str,
) -> Result<(&'a str, &'a [String]), OcaError> {
    let Some((first, rest)) = arguments.split_first() else {
        return Err(usage(format!("{name} is required")));
    };
    if first.starts_with('-') {
        return Err(usage(format!("{name} is required")));
    }
    Ok((first, rest))
}

fn parse_json_only(arguments: &[String]) -> Result<bool, OcaError> {
    let mut json = false;
    for argument in arguments {
        match argument.as_str() {
            "--json" => json = true,
            unknown => return Err(usage(format!("unknown flag `{unknown}`"))),
        }
    }
    Ok(json)
}

fn usage(error: impl Into<String>) -> OcaError {
    OcaError::new(ErrorCode::Usage).with_error(error)
}

#[cfg(test)]
mod tests {
    use super::{
        AbortCommand, AttachCommand, Command, DispatchCommand, EventsCommand, FollowCommand,
        ListCommand, MessageCommand, RefCommand, help_text, parse_from, public_commands,
    };
    use oca_core::ErrorCode;

    #[test]
    fn control_verbs_are_matched_before_model_dispatch() {
        assert_eq!(
            parse_from(["oca", "ls"]).expect("ls is a control verb"),
            Command::List(ListCommand::default()),
        );
    }

    #[test]
    fn control_commands_join_messages_and_keep_their_typed_values() {
        assert_eq!(
            parse_from(["oca", "m", "wabc12", "review", "this"]).unwrap(),
            Command::Message(MessageCommand {
                reference: "wabc12".to_owned(),
                message: "review this".to_owned(),
                effort: None,
                json: false,
            }),
        );
        assert_eq!(
            parse_from(["oca", "q", "wabc12", "after", "this"]).unwrap(),
            Command::Queue(MessageCommand {
                reference: "wabc12".to_owned(),
                message: "after this".to_owned(),
                effort: None,
                json: false,
            }),
        );
        assert_eq!(
            parse_from(["oca", "f", "wabc12", "--json"]).unwrap(),
            Command::Follow(FollowCommand {
                reference: "wabc12".to_owned(),
                json: true,
            }),
        );
        assert_eq!(
            parse_from(["oca", "k", "wabc12"]).unwrap(),
            Command::Abort(AbortCommand {
                reference: "wabc12".to_owned(),
                json: false,
            }),
        );
        assert_eq!(
            parse_from(["oca", "events", "wabc12", "--since", "7"]).unwrap(),
            Command::Events(EventsCommand {
                reference: "wabc12".to_owned(),
                since: Some(7),
                json: false,
            }),
        );
        assert_eq!(
            parse_from(["oca", "push", "wabc12"]).unwrap(),
            Command::Push(RefCommand {
                reference: "wabc12".to_owned(),
                json: false,
            }),
        );
        assert_eq!(
            parse_from(["oca", "pr", "wabc12"]).unwrap(),
            Command::PullRequest(RefCommand {
                reference: "wabc12".to_owned(),
                json: false,
            }),
        );
    }

    #[test]
    fn attach_is_parseable_but_not_an_agent_surface_command() {
        assert_eq!(
            parse_from(["oca", "__attach", "wabc12", "ses_1", "/repo"]).unwrap(),
            Command::Attach(AttachCommand {
                reference: "wabc12".to_owned(),
                session_id: "ses_1".to_owned(),
                cwd: "/repo".into(),
            }),
        );
    }

    #[test]
    fn model_dispatch_resolves_effort_and_joins_the_prompt() {
        let Command::Dispatch(command) = parse_from([
            "oca",
            "--json",
            "luna:h",
            "--role",
            "review",
            "-w",
            "--headless",
            "write",
            "a",
            "test",
        ])
        .unwrap() else {
            panic!("model grammar must produce dispatch");
        };

        assert_eq!(
            command,
            DispatchCommand {
                model: oca_core::resolve_model("luna", "h", oca_core::ModelCatalog::default())
                    .unwrap(),
                prompt: "write a test".to_owned(),
                role: "review".to_owned(),
                worktree: true,
                headless: true,
                json: true,
            }
        );
    }

    #[test]
    fn model_effort_can_be_separate_from_the_alias() {
        let Command::Dispatch(command) =
            parse_from(["oca", "sol", "-e", "x", "implement", "this"]).unwrap()
        else {
            panic!("model grammar must produce dispatch");
        };
        assert_eq!(command.model.alias, "sol");
        assert_eq!(command.model.variant, "xhigh");
        assert_eq!(command.prompt, "implement this");
    }

    #[test]
    fn parser_preserves_resolver_validation_order_without_network_work() {
        for (arguments, expected) in [
            (
                vec!["oca", "unknown", "-e", "h", "prompt"],
                ErrorCode::AliasUnknown,
            ),
            (vec!["oca", "luna", "prompt"], ErrorCode::EffortMissing),
            (
                vec!["oca", "luna:h", "-e", "low", "prompt"],
                ErrorCode::EffortConflict,
            ),
            (
                vec!["oca", "flash:low", "prompt"],
                ErrorCode::EffortUnsupported,
            ),
        ] {
            assert_eq!(parse_from(arguments).unwrap_err().code(), expected.as_str());
        }
    }

    #[test]
    fn end_of_options_keeps_a_dash_prefixed_prompt() {
        let Command::Dispatch(command) =
            parse_from(["oca", "luna:h", "--", "--write", "this"]).unwrap()
        else {
            panic!("model grammar must produce dispatch");
        };
        assert_eq!(command.prompt, "--write this");
        assert_eq!(
            parse_from(["oca", "luna:h", "--write", "this"])
                .unwrap_err()
                .code(),
            ErrorCode::Usage.as_str()
        );
    }

    #[test]
    fn retired_steer_token_is_never_a_command() {
        assert_eq!(
            parse_from(["oca", "s", "wabc12", "do", "this"])
                .unwrap_err()
                .code(),
            ErrorCode::Usage.as_str()
        );
    }

    #[test]
    fn resolver_errors_precede_a_missing_prompt() {
        assert_eq!(
            parse_from(["oca", "unknown"]).unwrap_err().code(),
            ErrorCode::AliasUnknown.as_str()
        );
        assert_eq!(
            parse_from(["oca", "luna"]).unwrap_err().code(),
            ErrorCode::EffortMissing.as_str()
        );
    }

    #[test]
    fn hidden_attach_is_excluded_from_agent_facing_metadata() {
        assert!(!help_text().contains("__attach"));
        assert!(!public_commands().contains(&"__attach"));
        assert_eq!(
            public_commands(),
            ["m", "q", "f", "k", "ls", "events", "push", "pr"]
        );
    }

    #[test]
    fn grammar_validation_failures_are_usage_envelopes() {
        let invalid = [
            vec!["oca"],
            vec!["oca", "luna:h"],
            vec!["oca", "luna:h", "--unknown", "prompt"],
            vec!["oca", "m"],
            vec!["oca", "m", "wabc12"],
            vec!["oca", "q", "wabc12", "-e", "h", "message"],
            vec!["oca", "f"],
            vec!["oca", "k"],
            vec!["oca", "events", "wabc12", "--since", "-1"],
            vec!["oca", "push"],
            vec!["oca", "pr"],
            vec!["oca", "__attach", "wabc12", "ses_1"],
        ];

        for arguments in invalid {
            assert_eq!(
                parse_from(arguments).unwrap_err().code(),
                ErrorCode::Usage.as_str()
            );
        }
    }
}
