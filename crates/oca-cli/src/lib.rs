use std::path::{Path, PathBuf};

use oca_core::{
    DEFAULT_MODEL_DEFINITIONS, EffortInput, ErrorCode, ModelCatalog, OcaError, RefId,
    ResolvedModel, resolve_model,
};
use oca_state::OcaConfig;

mod attach;
mod background;
mod control;
mod crash_recovery;
mod events;
mod follow;
mod foreground;
mod list;
mod publish;
mod pull_request;
mod scope;
mod transport;
mod worktree_dispatch;

pub use attach::execute_attach;
pub use background::execute_background;
pub use control::{ControlCommandOutput, execute_abort, execute_message, execute_queue};
pub use events::execute_events;
pub use follow::{FollowCommandOutput, execute_follow};
pub use foreground::execute_foreground;
pub use list::execute_list;
pub use publish::{PublishCommandOutput, execute_pull_request, execute_push};

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

/// The kind of agent-visible command described by [`AgentGrammar`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentCommand {
    /// A model alias and mandatory effort dispatch a new worker turn.
    Dispatch,
    /// A public control verb.
    Control(&'static str),
}

/// The required shape of an operand in an agent-visible command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperandForm {
    /// Exactly one token is required.
    Required,
    /// One or more tokens are required and are joined as text by the parser.
    OneOrMore,
}

/// One positional operand accepted by a command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperandGrammar {
    /// The display name used for the operand.
    pub name: &'static str,
    /// Whether the parser requires one token or a non-empty text tail.
    pub form: OperandForm,
}

/// The form of a value accepted after an option spelling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlagValueForm {
    /// The option is a switch and takes no following value.
    None,
    /// The option requires a following value with the displayed form.
    Required {
        /// The visible placeholder for the value.
        placeholder: &'static str,
        /// Enumerated values when the parser has a finite accepted set.
        accepted_values: &'static [&'static str],
    },
    /// The option requires a value from the selected alias's
    /// [`DispatchAliasGrammar::effort_ladder`].
    DispatchEffort {
        /// The visible placeholder for the effort value.
        placeholder: &'static str,
    },
}

/// The parser behavior controlled by an agent-visible flag.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum AgentFlag {
    /// Select machine-readable output.
    Json,
    /// Select model effort.
    Effort,
    /// Select the worker role.
    Role,
    /// Isolate worker edits in a worktree.
    Worktree,
    /// Return after background prompt admission and acknowledgement.
    Background,
    /// Run without an interactive terminal.
    Headless,
    /// Include completed workers in list output.
    All,
    /// Restrict list output to blocked workers.
    Blocked,
    /// Emit only the list result count.
    Count,
    /// Start event output at a cursor.
    Since,
    /// Bound a parked follow in seconds.
    Timeout,
}

/// One accepted option spelling and executable examples of its use.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FlagGrammar {
    /// The parser behavior controlled by this flag.
    pub kind: AgentFlag,
    /// Equivalent spellings accepted by the parser.
    pub spellings: &'static [&'static str],
    /// Whether the option takes a value and, if known, its accepted forms.
    pub value: FlagValueForm,
    /// Complete accepted argv sequences exercising the listed spellings.
    pub argv_examples: &'static [&'static [&'static str]],
}

/// The delimiter that stops option parsing for a trailing text operand.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EndOfOptionsGrammar {
    /// The exact delimiter token accepted by the parser.
    pub token: &'static str,
    /// The trailing operand that receives all subsequent tokens literally.
    pub trailing_operand: &'static str,
    /// Complete accepted argv sequences exercising the delimiter.
    pub argv_examples: &'static [&'static [&'static str]],
}

/// One model alias and its configured canonical effort ladder.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DispatchAliasGrammar {
    /// The exact alias spelling accepted in model-dispatch syntax.
    pub alias: &'static str,
    /// Canonical effort values that consumers can safely use for this alias.
    pub effort_ladder: &'static [&'static str],
}

/// The grammar for one public command spelling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommandGrammar {
    /// Whether this entry is dispatch syntax or one public control verb.
    pub kind: AgentCommand,
    /// Syntax tokens to show an agent when constructing the command.
    pub display_tokens: &'static [&'static str],
    /// Positional arguments accepted in order.
    pub operands: &'static [OperandGrammar],
    /// Accepted flags and their value forms.
    pub flags: &'static [FlagGrammar],
    /// A delimiter that makes the trailing text operand literal, when supported.
    pub end_of_options: Option<EndOfOptionsGrammar>,
    /// Complete accepted argv sequences for the command.
    pub argv_examples: &'static [&'static [&'static str]],
}

/// Parser-owned, agent-visible command grammar.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AgentGrammar {
    /// Every command agents may invoke, including model dispatch.
    pub commands: &'static [CommandGrammar],
    /// Every model alias accepted in model-dispatch syntax.
    pub dispatch_aliases: &'static [DispatchAliasGrammar],
    /// Every lexical spelling accepted for an effort value. Dispatch consumers
    /// should choose a canonical value from the selected alias's ladder.
    pub effort_forms: &'static [&'static str],
}

const EFFORT_FORMS: &[&str] = &["l", "m", "h", "x", "max", "low", "medium", "high", "xhigh"];

const fn dispatch_alias_count() -> usize {
    let mut count = DEFAULT_MODEL_DEFINITIONS.len();
    let mut definition_index = 0;
    while definition_index < DEFAULT_MODEL_DEFINITIONS.len() {
        count += DEFAULT_MODEL_DEFINITIONS[definition_index].synonyms.len();
        definition_index += 1;
    }
    count
}

const DISPATCH_ALIAS_COUNT: usize = dispatch_alias_count();

const fn derive_dispatch_aliases() -> [DispatchAliasGrammar; DISPATCH_ALIAS_COUNT] {
    let mut aliases = [DispatchAliasGrammar {
        alias: "",
        effort_ladder: &[],
    }; DISPATCH_ALIAS_COUNT];
    let mut definition_index = 0;
    let mut alias_index = 0;
    while definition_index < DEFAULT_MODEL_DEFINITIONS.len() {
        let definition = DEFAULT_MODEL_DEFINITIONS[definition_index];
        aliases[alias_index] = DispatchAliasGrammar {
            alias: definition.alias,
            effort_ladder: definition.ladder,
        };
        alias_index += 1;

        let mut synonym_index = 0;
        while synonym_index < definition.synonyms.len() {
            aliases[alias_index] = DispatchAliasGrammar {
                alias: definition.synonyms[synonym_index],
                effort_ladder: definition.ladder,
            };
            alias_index += 1;
            synonym_index += 1;
        }
        definition_index += 1;
    }
    aliases
}

const DISPATCH_ALIASES: [DispatchAliasGrammar; DISPATCH_ALIAS_COUNT] = derive_dispatch_aliases();
const DISPATCH_OPERANDS: &[OperandGrammar] = &[OperandGrammar {
    name: "prompt",
    form: OperandForm::OneOrMore,
}];
const REF_OPERAND: &[OperandGrammar] = &[OperandGrammar {
    name: "ref",
    form: OperandForm::Required,
}];
const REF_AND_MESSAGE_OPERANDS: &[OperandGrammar] = &[
    OperandGrammar {
        name: "ref",
        form: OperandForm::Required,
    },
    OperandGrammar {
        name: "message",
        form: OperandForm::OneOrMore,
    },
];

const DISPATCH_EXAMPLES: &[&[&str]] = &[
    &["oca", "luna:h", "implement", "the", "ticket"],
    &["oca", "sol", "-e", "x", "review", "the", "diff"],
    &["oca", "terra:medium", "summarize", "the", "change"],
    &["oca", "flash:max", "run", "the", "tests"],
    &["oca", "deepseek:h", "check", "the", "parser"],
    &["oca", "luna:h", "-b", "dispatch", "in", "background"],
    &["oca", "--json", "luna:h", "--", "--literal", "prompt"],
];
const DISPATCH_FLAGS: &[FlagGrammar] = &[
    FlagGrammar {
        kind: AgentFlag::Json,
        spellings: &["--json"],
        value: FlagValueForm::None,
        argv_examples: &[
            &["oca", "luna:h", "--json", "emit", "json"],
            &["oca", "--json", "luna:h", "emit", "global", "json"],
        ],
    },
    FlagGrammar {
        kind: AgentFlag::Effort,
        spellings: &["-e", "--effort"],
        value: FlagValueForm::DispatchEffort {
            placeholder: "<effort>",
        },
        argv_examples: &[
            &["oca", "sol", "-e", "high", "use", "short", "effort"],
            &["oca", "sol", "--effort", "h", "use", "long", "effort"],
        ],
    },
    FlagGrammar {
        kind: AgentFlag::Role,
        spellings: &["-r", "--role"],
        value: FlagValueForm::Required {
            placeholder: "<role>",
            accepted_values: &[],
        },
        argv_examples: &[
            &["oca", "terra:h", "-r", "review", "inspect", "this"],
            &["oca", "terra:h", "--role", "impl", "build", "this"],
        ],
    },
    FlagGrammar {
        kind: AgentFlag::Worktree,
        spellings: &["-w", "--worktree"],
        value: FlagValueForm::None,
        argv_examples: &[
            &["oca", "luna:h", "-w", "make", "an", "isolated", "change"],
            &["oca", "luna:h", "--worktree", "make", "another", "change"],
        ],
    },
    FlagGrammar {
        kind: AgentFlag::Background,
        spellings: &["-b"],
        value: FlagValueForm::None,
        argv_examples: &[&["oca", "luna:h", "-b", "dispatch", "without", "waiting"]],
    },
    FlagGrammar {
        kind: AgentFlag::Headless,
        spellings: &["--headless"],
        value: FlagValueForm::None,
        argv_examples: &[&["oca", "flash:h", "--headless", "run", "without", "a", "tui"]],
    },
];

const MESSAGE_EXAMPLES: &[&[&str]] = &[
    &["oca", "m", "w4f2a1", "continue", "the", "work"],
    &["oca", "m", "w4f2a1", "--", "--literal", "message"],
];
const MESSAGE_FLAGS: &[FlagGrammar] = &[
    FlagGrammar {
        kind: AgentFlag::Json,
        spellings: &["--json"],
        value: FlagValueForm::None,
        argv_examples: &[&["oca", "m", "w4f2a1", "--json", "report", "json"]],
    },
    FlagGrammar {
        kind: AgentFlag::Effort,
        spellings: &["-e", "--effort"],
        value: FlagValueForm::Required {
            placeholder: "<effort>",
            accepted_values: EFFORT_FORMS,
        },
        argv_examples: &[
            &["oca", "m", "w4f2a1", "-e", "h", "raise", "effort"],
            &["oca", "m", "w4f2a1", "--effort", "high", "raise", "effort"],
        ],
    },
];

const DISPATCH_END_OF_OPTIONS: EndOfOptionsGrammar = EndOfOptionsGrammar {
    token: "--",
    trailing_operand: "prompt",
    argv_examples: &[&["oca", "luna:h", "--", "--literal", "prompt"]],
};

const MESSAGE_END_OF_OPTIONS: EndOfOptionsGrammar = EndOfOptionsGrammar {
    token: "--",
    trailing_operand: "message",
    argv_examples: &[&["oca", "m", "w4f2a1", "--", "--literal", "message"]],
};

const QUEUE_EXAMPLES: &[&[&str]] = &[&["oca", "q", "w4f2a1", "queue", "this", "message"]];
const QUEUE_FLAGS: &[FlagGrammar] = &[FlagGrammar {
    kind: AgentFlag::Json,
    spellings: &["--json"],
    value: FlagValueForm::None,
    argv_examples: &[&["oca", "q", "w4f2a1", "--json", "queue", "json"]],
}];

const FOLLOW_EXAMPLES: &[&[&str]] = &[&["oca", "f", "w4f2a1"], &["oca", "f", "w4f2a1", "-t", "30"]];
const FOLLOW_FLAGS: &[FlagGrammar] = &[
    FlagGrammar {
        kind: AgentFlag::Timeout,
        spellings: &["-t"],
        value: FlagValueForm::Required {
            placeholder: "<seconds>",
            accepted_values: &[],
        },
        argv_examples: &[&["oca", "f", "w4f2a1", "-t", "30"]],
    },
    FlagGrammar {
        kind: AgentFlag::Json,
        spellings: &["--json"],
        value: FlagValueForm::None,
        argv_examples: &[&["oca", "f", "w4f2a1", "--json"]],
    },
];

const ABORT_EXAMPLES: &[&[&str]] = &[&["oca", "k", "w4f2a1"]];
const ABORT_FLAGS: &[FlagGrammar] = &[FlagGrammar {
    kind: AgentFlag::Json,
    spellings: &["--json"],
    value: FlagValueForm::None,
    argv_examples: &[&["oca", "k", "w4f2a1", "--json"]],
}];

const LIST_EXAMPLES: &[&[&str]] = &[&["oca", "ls"]];
const LIST_FLAGS: &[FlagGrammar] = &[
    FlagGrammar {
        kind: AgentFlag::All,
        spellings: &["--all"],
        value: FlagValueForm::None,
        argv_examples: &[&["oca", "ls", "--all"]],
    },
    FlagGrammar {
        kind: AgentFlag::Blocked,
        spellings: &["--blocked"],
        value: FlagValueForm::None,
        argv_examples: &[&["oca", "ls", "--blocked"]],
    },
    FlagGrammar {
        kind: AgentFlag::Count,
        spellings: &["--count"],
        value: FlagValueForm::None,
        argv_examples: &[&["oca", "ls", "--count"]],
    },
    FlagGrammar {
        kind: AgentFlag::Json,
        spellings: &["--json"],
        value: FlagValueForm::None,
        argv_examples: &[&["oca", "ls", "--json"]],
    },
];

const EVENTS_EXAMPLES: &[&[&str]] = &[&["oca", "events", "w4f2a1"]];
const EVENTS_FLAGS: &[FlagGrammar] = &[
    FlagGrammar {
        kind: AgentFlag::Since,
        spellings: &["--since"],
        value: FlagValueForm::Required {
            placeholder: "<non-negative-integer>",
            accepted_values: &[],
        },
        argv_examples: &[&["oca", "events", "w4f2a1", "--since", "7"]],
    },
    FlagGrammar {
        kind: AgentFlag::Json,
        spellings: &["--json"],
        value: FlagValueForm::None,
        argv_examples: &[&["oca", "events", "w4f2a1", "--json"]],
    },
];

const PUSH_EXAMPLES: &[&[&str]] = &[&["oca", "push", "w4f2a1"]];
const PUSH_FLAGS: &[FlagGrammar] = &[FlagGrammar {
    kind: AgentFlag::Json,
    spellings: &["--json"],
    value: FlagValueForm::None,
    argv_examples: &[&["oca", "push", "w4f2a1", "--json"]],
}];

const PULL_REQUEST_EXAMPLES: &[&[&str]] = &[&["oca", "pr", "w4f2a1"]];
const PULL_REQUEST_FLAGS: &[FlagGrammar] = &[FlagGrammar {
    kind: AgentFlag::Json,
    spellings: &["--json"],
    value: FlagValueForm::None,
    argv_examples: &[&["oca", "pr", "w4f2a1", "--json"]],
}];

const AGENT_COMMANDS: &[CommandGrammar] = &[
    CommandGrammar {
        kind: AgentCommand::Dispatch,
        display_tokens: &["<alias>:<effort>", "<alias> -e <effort>"],
        operands: DISPATCH_OPERANDS,
        flags: DISPATCH_FLAGS,
        end_of_options: Some(DISPATCH_END_OF_OPTIONS),
        argv_examples: DISPATCH_EXAMPLES,
    },
    CommandGrammar {
        kind: AgentCommand::Control("m"),
        display_tokens: &["m"],
        operands: REF_AND_MESSAGE_OPERANDS,
        flags: MESSAGE_FLAGS,
        end_of_options: Some(MESSAGE_END_OF_OPTIONS),
        argv_examples: MESSAGE_EXAMPLES,
    },
    CommandGrammar {
        kind: AgentCommand::Control("q"),
        display_tokens: &["q"],
        operands: REF_AND_MESSAGE_OPERANDS,
        flags: QUEUE_FLAGS,
        end_of_options: None,
        argv_examples: QUEUE_EXAMPLES,
    },
    CommandGrammar {
        kind: AgentCommand::Control("f"),
        display_tokens: &["f"],
        operands: REF_OPERAND,
        flags: FOLLOW_FLAGS,
        end_of_options: None,
        argv_examples: FOLLOW_EXAMPLES,
    },
    CommandGrammar {
        kind: AgentCommand::Control("k"),
        display_tokens: &["k"],
        operands: REF_OPERAND,
        flags: ABORT_FLAGS,
        end_of_options: None,
        argv_examples: ABORT_EXAMPLES,
    },
    CommandGrammar {
        kind: AgentCommand::Control("ls"),
        display_tokens: &["ls"],
        operands: &[],
        flags: LIST_FLAGS,
        end_of_options: None,
        argv_examples: LIST_EXAMPLES,
    },
    CommandGrammar {
        kind: AgentCommand::Control("events"),
        display_tokens: &["events"],
        operands: REF_OPERAND,
        flags: EVENTS_FLAGS,
        end_of_options: None,
        argv_examples: EVENTS_EXAMPLES,
    },
    CommandGrammar {
        kind: AgentCommand::Control("push"),
        display_tokens: &["push"],
        operands: REF_OPERAND,
        flags: PUSH_FLAGS,
        end_of_options: None,
        argv_examples: PUSH_EXAMPLES,
    },
    CommandGrammar {
        kind: AgentCommand::Control("pr"),
        display_tokens: &["pr"],
        operands: REF_OPERAND,
        flags: PULL_REQUEST_FLAGS,
        end_of_options: None,
        argv_examples: PULL_REQUEST_EXAMPLES,
    },
];

const PUBLIC_COMMAND_COUNT: usize = AGENT_COMMANDS.len() - 1;

const fn derive_public_commands() -> [&'static str; PUBLIC_COMMAND_COUNT] {
    let mut public_commands = [""; PUBLIC_COMMAND_COUNT];
    let mut command_index = 0;
    let mut public_index = 0;
    while command_index < AGENT_COMMANDS.len() {
        match AGENT_COMMANDS[command_index].kind {
            AgentCommand::Dispatch => {}
            AgentCommand::Control(verb) => {
                public_commands[public_index] = verb;
                public_index += 1;
            }
        }
        command_index += 1;
    }
    assert!(public_index == PUBLIC_COMMAND_COUNT);
    public_commands
}

const PUBLIC_COMMANDS: [&str; PUBLIC_COMMAND_COUNT] = derive_public_commands();

/// The public parser-owned grammar for agent construction of `oca` argv.
pub static AGENT_GRAMMAR: AgentGrammar = AgentGrammar {
    commands: AGENT_COMMANDS,
    dispatch_aliases: &DISPATCH_ALIASES,
    effort_forms: EFFORT_FORMS,
};

/// Returns the public parser-owned grammar for agent construction of `oca` argv.
#[must_use]
pub const fn grammar_contract() -> &'static AgentGrammar {
    &AGENT_GRAMMAR
}

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
     \nModel options: -e, --role, -w, -b, --headless, --json"
}

/// A model turn to dispatch to `OpenCode`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DispatchCommand {
    pub model: ResolvedModel,
    pub prompt: String,
    pub role: String,
    pub worktree: bool,
    pub background: bool,
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
    pub timeout_seconds: Option<u64>,
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
    pub display_name: String,
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
    parse_from_catalog(arguments, &ModelCatalog::default())
}

/// Parse an argv sequence using the configuration under `home/.oca`.
///
/// This is the production parser used by the `oca` binary. A missing config
/// file preserves the compiled-in model catalog.
///
/// # Errors
///
/// Returns a structured error when configuration cannot be loaded or the
/// command is invalid.
pub fn parse_from_home<I, S>(arguments: I, home: impl AsRef<Path>) -> Result<Command, OcaError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let config = OcaConfig::load_from_home(home).map_err(|error| {
        OcaError::new(ErrorCode::Usage)
            .with_error(format!("failed to load configuration: {error}"))
            .with_help("fix ~/.oca/config.toml and retry")
    })?;
    parse_from_catalog(arguments, &config.model_catalog())
}

fn parse_from_catalog<I, S>(arguments: I, catalog: &ModelCatalog) -> Result<Command, OcaError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut arguments = arguments.into_iter().map(Into::into);
    let _program_name = arguments.next();
    parse_arguments(arguments.collect(), catalog)
}

fn parse_arguments(
    mut arguments: Vec<String>,
    catalog: &ModelCatalog,
) -> Result<Command, OcaError> {
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
        _ => parse_dispatch(verb, tail, catalog).map(Command::Dispatch),
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

fn parse_dispatch(
    verb: &str,
    arguments: &[String],
    catalog: &ModelCatalog,
) -> Result<DispatchCommand, OcaError> {
    let (alias, inline_effort) = match verb.split_once(':') {
        Some((alias, effort)) => (alias, Some(effort.to_owned())),
        None => (verb, None),
    };
    let mut flagged_effort = None;
    let mut role = "impl".to_owned();
    let mut worktree = false;
    let mut background = false;
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
        } else if argument == "-b" {
            background = true;
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
        catalog,
    )?;
    if prompt.is_empty() {
        return Err(usage("a prompt is required"));
    }
    Ok(DispatchCommand {
        model,
        prompt: prompt.join(" "),
        role,
        worktree,
        background,
        headless,
        json,
    })
}

fn parse_message(arguments: &[String], permits_effort: bool) -> Result<MessageCommand, OcaError> {
    let (reference, tail) = required_reference(arguments)?;
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
            if !is_effort_form(value) {
                return Err(usage(format!("unsupported effort `{value}`")));
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

fn is_effort_form(value: &str) -> bool {
    let value = value.trim();
    EFFORT_FORMS
        .iter()
        .any(|effort| value.eq_ignore_ascii_case(effort))
}

fn parse_follow(arguments: &[String]) -> Result<FollowCommand, OcaError> {
    let (reference, tail) = required_reference(arguments)?;
    let mut timeout_seconds = None;
    let mut json = false;
    let mut index = 0;
    while index < tail.len() {
        match tail[index].as_str() {
            "--json" => json = true,
            "-t" => {
                index += 1;
                let Some(value) = tail.get(index) else {
                    return Err(usage("`-t` requires a timeout in seconds"));
                };
                let seconds = value
                    .parse::<u64>()
                    .ok()
                    .filter(|seconds| *seconds > 0)
                    .ok_or_else(|| usage("`-t` must be a positive integer number of seconds"))?;
                if timeout_seconds.replace(seconds).is_some() {
                    return Err(usage("`-t` was provided more than once"));
                }
            }
            unknown => return Err(usage(format!("unknown flag `{unknown}` for `f`"))),
        }
        index += 1;
    }
    Ok(FollowCommand {
        reference: reference.to_owned(),
        timeout_seconds,
        json,
    })
}

fn parse_abort(arguments: &[String]) -> Result<AbortCommand, OcaError> {
    let (reference, tail) = required_reference(arguments)?;
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
    let (reference, tail) = required_reference(arguments)?;
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
    let (reference, tail) = required_reference(arguments)?;
    Ok(RefCommand {
        reference: reference.to_owned(),
        json: parse_json_only(tail)?,
    })
}

fn parse_attach(arguments: &[String]) -> Result<AttachCommand, OcaError> {
    let (reference, tail) = required_reference(arguments)?;
    let (session_id, tail) = required_first(tail, "session id")?;
    let (cwd, tail) = required_first(tail, "cwd")?;
    let (display_name, tail) = required_first(tail, "display name")?;
    if !tail.is_empty() {
        return Err(usage(
            "`__attach` accepts exactly a ref, session id, cwd, and display name",
        ));
    }
    Ok(AttachCommand {
        reference: reference.to_owned(),
        session_id: session_id.to_owned(),
        cwd: cwd.into(),
        display_name: display_name.to_owned(),
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

fn required_reference(arguments: &[String]) -> Result<(&str, &[String]), OcaError> {
    let (reference, tail) = required_first(arguments, "ref")?;
    RefId::new(reference).map_err(|_| {
        usage("ref must be `w` followed by five lowercase ASCII base-36 characters")
    })?;
    Ok((reference, tail))
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
                timeout_seconds: None,
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
    fn follow_timeout_is_typed_and_validated_locally() {
        assert_eq!(
            parse_from(["oca", "f", "wabc12", "-t", "30"]).unwrap(),
            Command::Follow(FollowCommand {
                reference: "wabc12".to_owned(),
                timeout_seconds: Some(30),
                json: false,
            })
        );
        for arguments in [
            vec!["oca", "f", "wabc12", "-t"],
            vec!["oca", "f", "wabc12", "-t", "0"],
            vec!["oca", "f", "wabc12", "-t", "seconds"],
            vec!["oca", "f", "wabc12", "-t", "1", "-t", "2"],
        ] {
            assert_eq!(
                parse_from(arguments).unwrap_err().code_kind(),
                ErrorCode::Usage
            );
        }
    }

    #[test]
    fn attach_is_parseable_but_not_an_agent_surface_command() {
        assert_eq!(
            parse_from(["oca", "__attach", "wabc12", "ses_1", "/repo", "fixParser"]).unwrap(),
            Command::Attach(AttachCommand {
                reference: "wabc12".to_owned(),
                session_id: "ses_1".to_owned(),
                cwd: "/repo".into(),
                display_name: "fixParser".to_owned(),
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
                background: false,
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
    fn background_dispatch_is_a_typed_model_flag() {
        let Command::Dispatch(command) =
            parse_from(["oca", "luna:h", "-b", "implement", "this"]).unwrap()
        else {
            panic!("model grammar must produce dispatch");
        };
        assert!(command.background);
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
    fn retired_steer_token_is_not_recognized_as_a_control_verb() {
        assert_eq!(
            parse_from(["oca", "s", "wabc12", "do", "this"])
                .unwrap_err()
                .code(),
            ErrorCode::AliasUnknown.as_str()
        );
        assert!(!public_commands().contains(&"s"));
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
