use oca_cli::{
    AgentFlag, AgentGrammar, FlagGrammar, FlagValueForm, OperandForm, grammar_contract, parse_from,
};
use std::{fs, path::Path};

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Notification {
    Destructive,
    UnapprovedPublish,
}

#[cfg(test)]
fn classify_notification(
    event_type: &str,
    serialized_event: &str,
    pre_approved: bool,
) -> Option<Notification> {
    if !matches!(event_type, "permission.asked" | "tool.execute.before") {
        return None;
    }

    let event = serialized_event.to_ascii_lowercase();
    if is_destructive(&event) {
        return Some(Notification::Destructive);
    }
    if !pre_approved && is_publish_attempt(&event) {
        return Some(Notification::UnapprovedPublish);
    }
    None
}

#[cfg(test)]
fn is_destructive(event: &str) -> bool {
    [
        "rm -r",
        "rm -f",
        "rmdir",
        "truncate",
        "mkfs",
        "dd if=",
        "git reset --hard",
        "git clean -f",
        "git clean -d",
        "git clean -x",
        "drop table",
        "drop database",
    ]
    .iter()
    .any(|pattern| event.contains(pattern))
}

#[cfg(test)]
fn is_publish_attempt(event: &str) -> bool {
    ["git push", "gh pr", "gh repo", "git remote"]
        .iter()
        .any(|pattern| event.contains(pattern))
}

fn render_plugin() -> &'static str {
    r#"const destructive = /\b(rm\s+-[rf]{1,2}|rmdir|truncate|mkfs|dd\s+if=\S+|git\s+(reset\s+--hard|clean\s+-[fdx]+)|drop\s+(table|database))\b/i;
const publish = /\bgit\s+push\b|\bgh\s+(pr|repo)\b|\bgit\s+remote\b/i;

function classify(event) {
  const kind = event.type ?? "";
  if (kind !== "permission.asked" && kind !== "tool.execute.before") return null;
  const body = JSON.stringify(event.properties ?? event);
  if (destructive.test(body)) return "destructive action";
  if (publish.test(body) && event.properties?.preApproved !== true) return "unapproved publish";
  return null;
}

async function notify(title, body) {
  const ntfy = process.env.OCA_NTFY_URL;
  if (ntfy) await fetch(ntfy, { method: "POST", body: `${title}: ${body}` });
  if (process.env.OCA_DESKTOP_NOTIFY === "1")
    Bun.spawn(["notify-send", title, body], { stdout: "ignore", stderr: "ignore" });
}

export default async function OcaNotify() {
  return {
    event: async ({ event }) => {
      const reason = classify(event);
      if (!reason) return;
      await notify(`oca ${reason}`, `session=${event.properties?.sessionID ?? "unknown"}`);
    },
  };
}
"#
}

#[derive(Debug)]
struct GeneratedInvocation {
    display: String,
    argv: Vec<String>,
}

#[derive(Debug)]
struct AdvertisedFlag {
    spelling: &'static str,
    argv: Vec<String>,
}

#[derive(Debug)]
struct RenderedGuidance {
    artifacts: Vec<(&'static str, String)>,
    invocations: Vec<GeneratedInvocation>,
    advertised_flags: Vec<AdvertisedFlag>,
}

fn render_guidance(contract: &AgentGrammar) -> Result<RenderedGuidance, String> {
    let (skill, mut invocations, advertised_flags) = render_skill(contract)?;
    let (hook, hook_invocation) = render_hook(contract)?;
    invocations.push(hook_invocation);

    Ok(RenderedGuidance {
        artifacts: vec![
            ("skills/oca/SKILL.md", skill),
            ("templates/opencode-plugin.js", render_plugin().to_owned()),
            ("templates/hook.sh", hook),
        ],
        invocations,
        advertised_flags,
    })
}

fn render_hook(contract: &AgentGrammar) -> Result<(String, GeneratedInvocation), String> {
    let command = contract
        .commands
        .iter()
        .find(|command| {
            command
                .flags
                .iter()
                .any(|flag| flag.kind == AgentFlag::Blocked)
                && command
                    .flags
                    .iter()
                    .any(|flag| flag.kind == AgentFlag::Count)
        })
        .ok_or_else(|| "grammar contract has no blocked-count command".to_owned())?;
    let display_token = command
        .display_tokens
        .first()
        .ok_or_else(|| "blocked-count command has no display token".to_owned())?;
    let blocked = flag_spelling(command.flags, AgentFlag::Blocked)?;
    let count = flag_spelling(command.flags, AgentFlag::Count)?;
    let invocation = format!("oca {display_token} {blocked} {count}");
    let contents = format!(
        "#!/bin/sh\ncount=$({invocation})\nprev=$(cat \"$state_file\" 2>/dev/null || echo 0)\n[ \"$count\" = \"$prev\" ] && exit 0\nprintf 'oca inbox blocked=%s delta=%+d\\n' \"$count\" \"$((count-prev))\"\necho \"$count\" > \"$state_file\"\n"
    );

    Ok((
        contents,
        GeneratedInvocation {
            display: invocation,
            argv: vec![
                "oca".to_owned(),
                (*display_token).to_owned(),
                blocked.to_owned(),
                count.to_owned(),
            ],
        },
    ))
}

fn flag_spelling(flags: &[FlagGrammar], kind: AgentFlag) -> Result<&'static str, String> {
    flags
        .iter()
        .find(|flag| flag.kind == kind)
        .and_then(|flag| flag.spellings.first().copied())
        .ok_or_else(|| format!("grammar contract has no spelling for {kind:?}"))
}

fn render_skill(
    contract: &AgentGrammar,
) -> Result<(String, Vec<GeneratedInvocation>, Vec<AdvertisedFlag>), String> {
    let mut command_lines = Vec::new();
    let mut invocations = Vec::new();
    let mut advertised_flags = Vec::new();

    for command in contract.commands {
        let validation_argv = command.argv_examples.first().ok_or_else(|| {
            format!(
                "grammar command `{}` has no concrete argv example",
                command.display_tokens.join(" ")
            )
        })?;
        for (index, display_token) in command.display_tokens.iter().enumerate() {
            let line = render_command_line(display_token, command.operands, command.flags);
            command_lines.push(format!("    {line}"));
            let example = command.argv_examples.get(index).unwrap_or(validation_argv);
            invocations.push(GeneratedInvocation {
                display: line,
                argv: example.iter().map(|token| (*token).to_owned()).collect(),
            });
        }

        for flag in command.flags {
            for spelling in flag.spellings {
                let example = flag
                    .argv_examples
                    .iter()
                    .find(|example| example.contains(spelling))
                    .ok_or_else(|| {
                        format!(
                            "advertised flag `{spelling}` for `{}` has no concrete argv example",
                            command.display_tokens.join(" ")
                        )
                    })?;
                advertised_flags.push(AdvertisedFlag {
                    spelling,
                    argv: example.iter().map(|token| (*token).to_owned()).collect(),
                });
            }
        }
    }

    let aliases = contract
        .dispatch_aliases
        .iter()
        .map(|alias| format!("{}: {}", alias.alias, alias.effort_ladder.join(" ")))
        .collect::<Vec<_>>()
        .join("; ");
    let skill = format!(
        "---\nname: oca\ndescription: Delegate engineering work to OpenCode workers and inspect or steer their state.\n---\n\n# oca\n\nDispatch every worker with an explicit alias and effort. There is no default.\n\nAliases and canonical effort ladders: {aliases}.\nAccepted effort forms: {}.\n\nConstruct commands only from this parser-owned surface:\n\n{}\n\nUse the follow command when waiting for a worker. It exits 0 done, 3 blocked, 4 timeout, and 5 server unreachable.\n\nUse the worktree option when edits must stay isolated. Workers never run git; oca validates the diff and commits locally after each worktree turn.\n\nA blocked worker ends its turn with a question; answer it with the message command. Publication commands work only where the repository publish grant allows them. Merges and grants stay with the human.\n",
        contract.effort_forms.join(" "),
        command_lines.join("\n")
    );

    Ok((skill, invocations, advertised_flags))
}

fn render_command_line(
    display_token: &str,
    operands: &[oca_cli::OperandGrammar],
    flags: &[FlagGrammar],
) -> String {
    let mut tokens = vec!["oca".to_owned(), display_token.to_owned()];
    tokens.extend(
        operands
            .iter()
            .filter(|operand| operand.form == OperandForm::Required)
            .map(|operand| format!("<{}>", operand.name)),
    );
    tokens.extend(
        flags
            .iter()
            .filter(|flag| {
                !matches!(flag.value, FlagValueForm::DispatchEffort { .. })
                    || !display_token.contains("<effort>")
            })
            .map(render_flag),
    );
    tokens.extend(
        operands
            .iter()
            .filter(|operand| operand.form == OperandForm::OneOrMore)
            .map(|operand| format!("<{}...>", operand.name)),
    );
    tokens.join(" ")
}

fn render_flag(flag: &FlagGrammar) -> String {
    let value = match flag.value {
        FlagValueForm::None => String::new(),
        FlagValueForm::Required { placeholder, .. }
        | FlagValueForm::DispatchEffort { placeholder } => format!(" {placeholder}"),
    };
    format!("[{}{value}]", flag.spellings.join("|"))
}

fn validate_rendered_guidance(guidance: &RenderedGuidance) -> Result<(), String> {
    for invocation in &guidance.invocations {
        parse_from(invocation.argv.iter().cloned()).map_err(|error| {
            format!(
                "generated invocation `{}` was rejected by oca_cli::parse_from: {error}",
                invocation.display
            )
        })?;
    }
    for flag in &guidance.advertised_flags {
        parse_from(flag.argv.iter().cloned()).map_err(|error| {
            format!(
                "advertised flag `{}` was rejected by oca_cli::parse_from: {error}",
                flag.spelling
            )
        })?;
    }
    Ok(())
}

pub(crate) fn write_artifacts(workspace_root: &Path) -> Result<(), String> {
    let guidance = render_guidance(grammar_contract())?;
    validate_rendered_guidance(&guidance)?;
    for (relative_path, contents) in guidance.artifacts {
        let path = workspace_root.join(relative_path);
        let parent = path
            .parent()
            .ok_or_else(|| format!("generated artifact has no parent: {relative_path}"))?;
        fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
        fs::write(&path, contents)
            .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    }
    Ok(())
}

pub(crate) fn check_drift(workspace_root: &Path) -> Result<(), String> {
    check_drift_with_contract(workspace_root, grammar_contract())
}

fn check_drift_with_contract(workspace_root: &Path, contract: &AgentGrammar) -> Result<(), String> {
    let guidance = render_guidance(contract)?;
    validate_rendered_guidance(&guidance)?;

    let mut drifted = Vec::new();
    for (relative_path, contents) in guidance.artifacts {
        let path = workspace_root.join(relative_path);
        match fs::read_to_string(&path) {
            Ok(committed) if committed == contents => {}
            Ok(_) | Err(_) => drifted.push(relative_path),
        }
    }

    if drifted.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "generated artifacts are out of sync: {}; run `cargo xtask generate-guidance`",
            drifted.join(", ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Notification, check_drift_with_contract, classify_notification, render_hook, render_plugin,
        render_skill,
    };
    use oca_cli::{
        AgentCommand, AgentFlag, AgentGrammar, CommandGrammar, DispatchAliasGrammar, FlagGrammar,
        FlagValueForm, OperandForm, OperandGrammar, grammar_contract,
    };
    use std::{env, fs, process, time::SystemTime};

    const FIXTURE_DISPATCH_FLAGS: &[FlagGrammar] = &[FlagGrammar {
        kind: AgentFlag::Role,
        spellings: &["--persona"],
        value: FlagValueForm::Required {
            placeholder: "<persona>",
            accepted_values: &[],
        },
        argv_examples: &[&["oca", "unit:h", "--persona", "impl", "work"]],
    }];
    const FIXTURE_LIST_FLAGS: &[FlagGrammar] = &[
        FlagGrammar {
            kind: AgentFlag::Blocked,
            spellings: &["--stalled"],
            value: FlagValueForm::None,
            argv_examples: &[&["oca", "inbox-token", "--stalled"]],
        },
        FlagGrammar {
            kind: AgentFlag::Count,
            spellings: &["--total"],
            value: FlagValueForm::None,
            argv_examples: &[&["oca", "inbox-token", "--total"]],
        },
    ];
    const FIXTURE_COMMANDS: &[CommandGrammar] = &[
        CommandGrammar {
            kind: AgentCommand::Dispatch,
            display_tokens: &["<engine>@<depth>"],
            operands: &[OperandGrammar {
                name: "job",
                form: OperandForm::OneOrMore,
            }],
            flags: FIXTURE_DISPATCH_FLAGS,
            end_of_options: None,
            argv_examples: &[&["oca", "unit:h", "work"]],
        },
        CommandGrammar {
            kind: AgentCommand::Control("inbox"),
            display_tokens: &["inbox-token"],
            operands: &[],
            flags: FIXTURE_LIST_FLAGS,
            end_of_options: None,
            argv_examples: &[&["oca", "inbox-token"]],
        },
    ];
    const FIXTURE_ALIASES: &[DispatchAliasGrammar] = &[DispatchAliasGrammar {
        alias: "unit",
        effort_ladder: &["h"],
    }];
    const RENDER_FIXTURE: AgentGrammar = AgentGrammar {
        commands: FIXTURE_COMMANDS,
        dispatch_aliases: FIXTURE_ALIASES,
        effort_forms: &["h"],
    };

    const REJECTED_DISPATCH_FLAGS: &[FlagGrammar] = &[FlagGrammar {
        kind: AgentFlag::Worktree,
        spellings: &["-b"],
        value: FlagValueForm::None,
        argv_examples: &[&["oca", "luna:l", "-b", "work"]],
    }];
    const REJECTED_LIST_FLAGS: &[FlagGrammar] = &[
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
    ];
    const REJECTED_COMMANDS: &[CommandGrammar] = &[
        CommandGrammar {
            kind: AgentCommand::Dispatch,
            display_tokens: &["<alias>:<effort>"],
            operands: &[OperandGrammar {
                name: "prompt",
                form: OperandForm::OneOrMore,
            }],
            flags: REJECTED_DISPATCH_FLAGS,
            end_of_options: None,
            argv_examples: &[&["oca", "luna:l", "-b", "work"]],
        },
        CommandGrammar {
            kind: AgentCommand::Control("ls"),
            display_tokens: &["ls"],
            operands: &[],
            flags: REJECTED_LIST_FLAGS,
            end_of_options: None,
            argv_examples: &[&["oca", "ls"]],
        },
    ];
    const REJECTED_CONTRACT: AgentGrammar = AgentGrammar {
        commands: REJECTED_COMMANDS,
        dispatch_aliases: &[DispatchAliasGrammar {
            alias: "luna",
            effort_ladder: &["l"],
        }],
        effort_forms: &["l"],
    };

    // Seam under test: classify_notification. The generated plugin must only
    // surface destructive work and unapproved publication attempts.
    #[test]
    fn classifier_only_notifies_for_destructive_or_unapproved_publish_work() {
        let positives = [
            (
                "tool.execute.before",
                "{\"command\":\"rm -rf generated\"}",
                false,
                Some(Notification::Destructive),
            ),
            (
                "permission.asked",
                "{\"command\":\"git push origin oca/worker\"}",
                false,
                Some(Notification::UnapprovedPublish),
            ),
            (
                "tool.execute.before",
                "{\"command\":\"gh pr create\"}",
                false,
                Some(Notification::UnapprovedPublish),
            ),
            (
                "tool.execute.before",
                "{\"command\":\"gh repo create sample\"}",
                false,
                Some(Notification::UnapprovedPublish),
            ),
            (
                "tool.execute.before",
                "{\"command\":\"git remote add upstream git@example.com:team/repo\"}",
                false,
                Some(Notification::UnapprovedPublish),
            ),
        ];
        for (event_type, body, pre_approved, expected) in positives {
            assert_eq!(
                classify_notification(event_type, body, pre_approved),
                expected,
                "fixture: {body}"
            );
        }

        let negatives = [
            ("tool.execute.before", "{\"command\":\"cargo test\"}", false),
            ("session.idle", "{\"status\":\"completed\"}", false),
            ("permission.asked", "{\"question\":\"continue?\"}", false),
            (
                "tool.execute.before",
                "{\"command\":\"git push origin oca/worker\"}",
                true,
            ),
        ];
        for (event_type, body, pre_approved) in negatives {
            assert_eq!(
                classify_notification(event_type, body, pre_approved),
                None,
                "fixture: {body}"
            );
        }
    }

    #[test]
    fn plugin_renderer_emits_the_escalation_only_plugin() {
        let plugin = render_plugin();

        assert!(plugin.contains("const destructive = /\\b(rm\\s+-[rf]"));
        assert!(plugin.contains("const publish = /\\bgit\\s+push\\b"));
        assert!(
            plugin.contains("kind !== \"permission.asked\" && kind !== \"tool.execute.before\"")
        );
        assert!(plugin.contains("event.properties?.preApproved !== true"));
        assert!(plugin.contains("OCA_NTFY_URL"));
        assert!(plugin.contains("OCA_DESKTOP_NOTIFY"));
        assert!(plugin.contains("export default async function OcaNotify()"));
        assert!(!plugin.contains("session.idle"));
    }

    #[test]
    fn hook_renderer_emits_a_delta_only_blocked_inbox() {
        let (hook, _) = render_hook(grammar_contract()).expect("default hook grammar is complete");

        assert!(hook.starts_with("#!/bin/sh\n"));
        assert!(hook.contains("count=$(oca ls --blocked --count)"));
        assert!(hook.contains("prev=$(cat \"$state_file\" 2>/dev/null || echo 0)"));
        assert!(hook.contains("[ \"$count\" = \"$prev\" ] && exit 0"));
        assert!(
            hook.contains(
                "printf 'oca inbox blocked=%s delta=%+d\\n' \"$count\" \"$((count-prev))\""
            )
        );
        assert!(!hook.contains("refs"));
        assert!(hook.ends_with("echo \"$count\" > \"$state_file\"\n"));
    }

    #[test]
    fn injected_contract_drives_skill_and_hook_tokens_and_flag_spellings() {
        let (skill, _, _) = render_skill(&RENDER_FIXTURE).expect("fixture renders a skill");
        let (hook, _) = render_hook(&RENDER_FIXTURE).expect("fixture renders a hook");

        assert!(skill.contains("oca <engine>@<depth> [--persona <persona>] <job...>"));
        assert!(skill.contains("oca inbox-token [--stalled] [--total]"));
        assert!(hook.contains("count=$(oca inbox-token --stalled --total)"));
    }

    #[test]
    fn skill_publishes_only_contract_commands_and_flags() {
        let (skill, _, _) =
            render_skill(grammar_contract()).expect("default grammar renders a skill");
        assert!(skill.starts_with("---\nname: oca\ndescription: Delegate engineering work"));
        assert!(skill.contains("luna: low medium high xhigh max"));
        assert!(skill.contains("It exits 0 done, 3 blocked, 4 timeout"));
        assert!(skill.contains("Merges and grants stay with the human."));
        for command in grammar_contract().commands {
            for display_token in command.display_tokens {
                assert!(skill.contains(&format!("oca {display_token}")));
            }
            for flag in command.flags {
                for spelling in flag.spellings {
                    assert!(skill.contains(spelling));
                }
            }
        }
        for forbidden in [
            "oca luna:h",
            " -b",
            "oca s ",
            "oca __attach",
            "oca clean",
            "oca ping",
        ] {
            assert!(!skill.contains(forbidden), "skill must exclude {forbidden}");
        }
    }

    #[test]
    fn drift_check_validates_parser_evidence_before_comparing_artifacts() {
        let workspace_root = env::temp_dir().join(format!(
            "oca-guidance-parser-first-test-{}-{}",
            process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("system time is after the Unix epoch")
                .as_nanos()
        ));
        fs::create_dir_all(workspace_root.join("skills/oca"))
            .expect("drifting artifact root can be created");
        fs::write(workspace_root.join("skills/oca/SKILL.md"), "drifted\n")
            .expect("drifting artifact can be written");

        let error = check_drift_with_contract(&workspace_root, &REJECTED_CONTRACT)
            .expect_err("the rejected -b fixture must fail parser validation");

        assert!(error.contains("oca_cli::parse_from"), "{error}");
        assert!(error.contains("unknown flag `-b`"), "{error}");
        assert!(!error.contains("out of sync"), "{error}");
        fs::remove_dir_all(workspace_root).expect("test output can be removed");
    }
}
