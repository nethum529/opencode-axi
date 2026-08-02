//! The published grammar must remain executable by the parser that owns it.

use oca_cli::{AgentCommand, FlagValueForm, grammar_contract, parse_from, public_commands};

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
    assert_eq!(controls, public_commands().collect::<Vec<_>>());
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
            parse_from(arguments.iter().copied()).unwrap_or_else(|error| {
                panic!("grammar example `{arguments:?}` must parse, got {error}")
            });
        }
        for flag in command.flags {
            assert!(
                !flag.spellings.is_empty(),
                "every advertised flag needs a spelling"
            );
            if let FlagValueForm::Required { placeholder, .. } = flag.value {
                assert!(
                    !placeholder.is_empty(),
                    "value-taking flags need a visible placeholder"
                );
            }
            for arguments in flag.argv_examples {
                parse_from(arguments.iter().copied()).unwrap_or_else(|error| {
                    panic!("flag example `{arguments:?}` must parse, got {error}")
                });
            }
        }
    }

    let dispatch = contract
        .commands
        .iter()
        .find(|command| command.kind == AgentCommand::Dispatch)
        .expect("the agent grammar includes model dispatch");
    for arguments in dispatch.argv_examples {
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
