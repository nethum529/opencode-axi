//! The published grammar must remain executable by the parser that owns it.

use std::collections::BTreeSet;

use oca_cli::{
    AgentCommand, Command, CommandGrammar, DispatchAliasGrammar, EndOfOptionsGrammar, FlagGrammar,
    FlagValueForm, OperandForm, grammar_contract, parse_from, public_commands,
};
use oca_core::{DEFAULT_MODEL_DEFINITIONS, ErrorCode};

const PUBLIC_COMMANDS: &[&str] = public_commands();

#[test]
fn agent_grammar_contract_is_visible_and_parseable() {
    let contract = grammar_contract();
    let controls = contract
        .commands
        .iter()
        .filter_map(|command| match command.kind {
            AgentCommand::Control(verb) => Some(verb),
            AgentCommand::Dispatch => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(
        controls,
        ["m", "q", "f", "k", "ls", "events", "push", "pr"],
        "the contract contains every and only public control verb"
    );
    assert_eq!(controls, PUBLIC_COMMANDS);
    assert_eq!(
        contract
            .commands
            .iter()
            .filter(|command| command.kind == AgentCommand::Dispatch)
            .count(),
        1,
        "model dispatch is the only non-control agent command"
    );
    assert!(
        contract
            .commands
            .iter()
            .all(|command| !command.display_tokens.contains(&"__attach")),
        "the detached display helper is not agent-visible"
    );

    for command in contract.commands {
        assert!(
            !command.display_tokens.is_empty(),
            "every command has a display spelling"
        );

        for arguments in command.argv_examples {
            assert_example_matches_command(command, arguments);
        }
        for flag in command.flags {
            assert!(
                !flag.spellings.is_empty(),
                "every advertised flag needs a spelling"
            );
            match flag.value {
                FlagValueForm::None => {}
                FlagValueForm::Required { placeholder, .. }
                | FlagValueForm::DispatchEffort { placeholder } => assert!(
                    !placeholder.is_empty(),
                    "value-taking flags need a visible placeholder"
                ),
            }
            assert_flag_examples_match_command(command, flag);
        }
        if let Some(end_of_options) = command.end_of_options {
            assert_end_of_options_matches_command(command, &end_of_options);
        }
    }

    assert_dispatch_contract_matches_resolver();
    assert_message_efforts_are_restricted_to_published_forms();
}

fn assert_dispatch_contract_matches_resolver() {
    let contract = grammar_contract();
    let dispatch = contract
        .commands
        .iter()
        .find(|command| command.kind == AgentCommand::Dispatch)
        .expect("the agent grammar includes model dispatch");
    let dispatch_examples = dispatch
        .argv_examples
        .iter()
        .copied()
        .chain(
            dispatch
                .flags
                .iter()
                .flat_map(|flag| flag.argv_examples.iter().copied()),
        )
        .chain(
            dispatch
                .end_of_options
                .iter()
                .flat_map(|grammar| grammar.argv_examples.iter().copied()),
        )
        .collect::<Vec<_>>();

    for arguments in &dispatch_examples {
        let (alias, effort) = dispatch_pair(arguments, contract.dispatch_aliases);
        assert!(
            contract
                .dispatch_aliases
                .iter()
                .any(|published| published.alias == alias),
            "dispatch example `{arguments:?}` uses an unpublished alias `{alias}`"
        );
        assert!(
            contract.effort_forms.contains(&effort),
            "dispatch example `{arguments:?}` uses an unpublished effort `{effort}`"
        );
    }

    let expected_aliases = DEFAULT_MODEL_DEFINITIONS
        .iter()
        .flat_map(|definition| {
            std::iter::once((definition.alias, definition.ladder)).chain(
                definition
                    .synonyms
                    .iter()
                    .copied()
                    .map(|synonym| (synonym, definition.ladder)),
            )
        })
        .collect::<Vec<_>>();
    let published_aliases = contract
        .dispatch_aliases
        .iter()
        .map(|published| (published.alias, published.effort_ladder))
        .collect::<Vec<_>>();
    assert_eq!(
        published_aliases
            .iter()
            .map(|(alias, _)| *alias)
            .collect::<BTreeSet<_>>()
            .len(),
        published_aliases.len(),
        "dispatch aliases must not contain duplicates"
    );
    assert_eq!(
        published_aliases, expected_aliases,
        "dispatch aliases and ladders must be derived from the default model definitions"
    );

    for published in contract.dispatch_aliases {
        assert!(
            dispatch_examples.iter().any(|arguments| {
                dispatch_pair(arguments, contract.dispatch_aliases).0 == published.alias
            }),
            "dispatch alias `{}` needs an executable published example",
            published.alias
        );

        assert!(
            !published.effort_ladder.is_empty(),
            "dispatch aliases need at least one effort"
        );

        for effort in published.effort_ladder {
            assert!(
                contract.effort_forms.contains(effort),
                "the `{}` ladder uses unpublished effort `{effort}`",
                published.alias
            );
            let derived = [
                "oca",
                published.alias,
                "-e",
                *effort,
                "derived",
                "contract",
                "example",
            ];
            assert_example_matches_command(dispatch, &derived);
        }
    }
}

fn assert_message_efforts_are_restricted_to_published_forms() {
    for effort in grammar_contract().effort_forms {
        parse_from(["oca", "m", "w4f2a1", "-e", effort, "message"])
            .unwrap_or_else(|error| panic!("published message effort `{effort}` failed: {error}"));
    }
    assert_eq!(
        parse_from(["oca", "m", "w4f2a1", "-e", "bogus", "message"])
            .unwrap_err()
            .code(),
        ErrorCode::Usage.as_str(),
        "message effort parsing must reject values outside effort_forms"
    );
}

fn assert_flag_examples_match_command(command: &CommandGrammar, flag: &FlagGrammar) {
    for spelling in flag.spellings {
        let arguments = flag
            .argv_examples
            .iter()
            .find(|arguments| arguments.contains(spelling))
            .unwrap_or_else(|| {
                panic!(
                    "flag `{spelling}` for `{}` has no executable example",
                    command.display_tokens.join(" ")
                )
            });

        assert_example_matches_command(command, arguments);

        let FlagValueForm::Required {
            accepted_values, ..
        } = flag.value
        else {
            continue;
        };
        let value_index = arguments
            .iter()
            .position(|argument| argument == spelling)
            .expect("the selected flag example contains its spelling")
            + 1;
        let example_value = arguments
            .get(value_index)
            .expect("value-taking flag examples provide a value");
        assert!(
            accepted_values.is_empty() || accepted_values.contains(example_value),
            "flag example `{arguments:?}` uses unadvertised value `{example_value}`"
        );

        for value in accepted_values {
            let mut derived = arguments.to_vec();
            derived[value_index] = value;
            assert_example_matches_command(command, &derived);
        }
    }
}

fn assert_end_of_options_matches_command(
    command: &CommandGrammar,
    end_of_options: &EndOfOptionsGrammar,
) {
    assert_eq!(end_of_options.token, "--");
    assert_eq!(
        command
            .operands
            .last()
            .expect("end-of-options needs a trailing operand")
            .name,
        end_of_options.trailing_operand
    );
    assert!(
        !end_of_options.argv_examples.is_empty(),
        "end-of-options needs an executable example"
    );

    for arguments in end_of_options.argv_examples {
        let delimiter_index = arguments
            .iter()
            .position(|argument| *argument == end_of_options.token)
            .expect("end-of-options examples contain the delimiter");
        let literal = arguments
            .get(delimiter_index + 1)
            .expect("end-of-options examples contain a literal trailing token");
        assert!(
            literal.starts_with('-'),
            "the example must prove a dash-prefixed token becomes literal"
        );

        let parsed = assert_example_matches_command(command, arguments);
        let trailing_text = match parsed {
            Command::Dispatch(dispatch) => dispatch.prompt,
            Command::Message(message) => message.message,
            other => panic!("unexpected end-of-options command: {other:?}"),
        };
        assert!(
            trailing_text
                .split_whitespace()
                .any(|token| token == *literal),
            "the delimiter must preserve `{literal}` in the trailing operand"
        );
    }
}

fn assert_example_matches_command(command: &CommandGrammar, arguments: &[&str]) -> Command {
    let parsed = parse_from(arguments.iter().copied())
        .unwrap_or_else(|error| panic!("grammar example `{arguments:?}` must parse, got {error}"));

    match (command.kind, &parsed) {
        (AgentCommand::Dispatch, Command::Dispatch(dispatch)) => {
            assert_eq!(command.operands.len(), 1);
            assert_eq!(command.operands[0].form, OperandForm::OneOrMore);
            assert!(
                !dispatch.prompt.is_empty(),
                "dispatch example `{arguments:?}` supplies the required prompt"
            );
        }
        (AgentCommand::Control("m"), Command::Message(message))
        | (AgentCommand::Control("q"), Command::Queue(message)) => {
            assert_eq!(command.operands.len(), 2);
            assert_eq!(command.operands[0].form, OperandForm::Required);
            assert_eq!(command.operands[1].form, OperandForm::OneOrMore);
            assert!(!message.reference.is_empty());
            assert!(!message.message.is_empty());
        }
        (AgentCommand::Control("f"), Command::Follow(follow)) => {
            assert_ref_operand(command, &follow.reference);
        }
        (AgentCommand::Control("k"), Command::Abort(abort)) => {
            assert_ref_operand(command, &abort.reference);
        }
        (AgentCommand::Control("ls"), Command::List(_)) => {
            assert!(command.operands.is_empty());
        }
        (AgentCommand::Control("events"), Command::Events(events)) => {
            assert_ref_operand(command, &events.reference);
        }
        (AgentCommand::Control("push"), Command::Push(push))
        | (AgentCommand::Control("pr"), Command::PullRequest(push)) => {
            assert_ref_operand(command, &push.reference);
        }
        (kind, parsed) => panic!(
            "grammar command `{}` produced {parsed:?}, not `{kind:?}`",
            command.display_tokens.join(" ")
        ),
    }
    parsed
}

fn assert_ref_operand(command: &CommandGrammar, reference: &str) {
    assert_eq!(command.operands.len(), 1);
    assert_eq!(command.operands[0].form, OperandForm::Required);
    assert!(!reference.is_empty());
}

fn dispatch_pair<'a>(
    arguments: &'a [&'a str],
    dispatch_aliases: &[DispatchAliasGrammar],
) -> (&'a str, &'a str) {
    let (verb_index, alias, inline_effort) = arguments
        .iter()
        .enumerate()
        .find_map(|(index, argument)| {
            dispatch_aliases.iter().find_map(|published| {
                if *argument == published.alias {
                    return Some((index, *argument, None));
                }
                let (alias, effort) = argument.split_once(':')?;
                (alias == published.alias).then_some((index, alias, Some(effort)))
            })
        })
        .expect("dispatch examples contain a published model spelling");
    if let Some(effort) = inline_effort {
        return (alias, effort);
    }

    let effort_index = arguments[verb_index + 1..]
        .iter()
        .position(|argument| *argument == "-e" || *argument == "--effort")
        .expect("dispatch examples without inline effort provide an effort flag")
        + verb_index
        + 1;
    (alias, arguments[effort_index + 1])
}
