//! The published grammar must remain executable by the parser that owns it.

use oca_cli::{
    AgentCommand, Command, CommandGrammar, FlagGrammar, FlagValueForm, OperandForm,
    grammar_contract, parse_from, public_commands,
};

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
    assert_eq!(controls, public_commands());
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
            if let FlagValueForm::Required {
                placeholder,
                accepted_values: _,
            } = flag.value
            {
                assert!(
                    !placeholder.is_empty(),
                    "value-taking flags need a visible placeholder"
                );
            }
            assert_flag_examples_match_command(command, flag);
        }
    }

    let dispatch = contract
        .commands
        .iter()
        .find(|command| command.kind == AgentCommand::Dispatch)
        .expect("the agent grammar includes model dispatch");
    for arguments in dispatch
        .argv_examples
        .iter()
        .chain(dispatch.flags.iter().flat_map(|flag| flag.argv_examples))
    {
        let (alias, effort) = dispatch_pair(arguments);
        assert!(
            contract.dispatch_aliases.contains(&alias),
            "dispatch example `{arguments:?}` uses an unpublished alias `{alias}`"
        );
        assert!(
            contract.effort_forms.contains(&effort),
            "dispatch example `{arguments:?}` uses an unpublished effort `{effort}`"
        );
    }
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

fn assert_example_matches_command(command: &CommandGrammar, arguments: &[&str]) {
    let parsed = parse_from(arguments.iter().copied())
        .unwrap_or_else(|error| panic!("grammar example `{arguments:?}` must parse, got {error}"));

    match (command.kind, parsed) {
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
}

fn assert_ref_operand(command: &CommandGrammar, reference: &str) {
    assert_eq!(command.operands.len(), 1);
    assert_eq!(command.operands[0].form, OperandForm::Required);
    assert!(!reference.is_empty());
}

fn dispatch_pair<'a>(arguments: &'a [&'a str]) -> (&'a str, &'a str) {
    let verb_index = arguments
        .iter()
        .position(|argument| *argument != "oca" && *argument != "--json")
        .expect("dispatch examples contain a model spelling");
    let model = arguments[verb_index];
    if let Some((alias, effort)) = model.split_once(':') {
        return (alias, effort);
    }

    let effort_index = arguments
        .iter()
        .position(|argument| *argument == "-e" || *argument == "--effort")
        .expect("dispatch examples without inline effort provide an effort flag");
    (model, arguments[effort_index + 1])
}
