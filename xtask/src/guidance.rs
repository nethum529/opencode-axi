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
    r#"const destructive = /\b(rm\s+-[rf]|rmdir|truncate|mkfs|dd\s+if=|git\s+(reset\s+--hard|clean\s+-[fdx])|drop\s+(table|database))\b/i;
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

fn render_hook() -> &'static str {
    r#"#!/bin/sh
count=$(oca ls --blocked --count)
prev=$(cat "$state_file" 2>/dev/null || echo 0)
[ "$count" = "$prev" ] && exit 0
printf 'oca inbox blocked=%s delta=%+d refs=%s\n' "$count" "$((count-prev))" "$(oca ls --blocked --count --refs)"
echo "$count" > "$state_file"
"#
}

const COMMANDS: &[&str] = &[
    "oca luna:h \"<task>\"",
    "oca flash:h -r impl -w -b \"<task>\"",
    "oca m <ref> \"<message>\"",
    "oca s <ref> \"<message>\"",
    "oca q <ref> \"<message>\"",
    "oca f <ref>",
    "oca k <ref>",
    "oca ls",
    "oca events <ref>",
    "oca push <ref>",
    "oca pr <ref>",
];

fn render_skill() -> String {
    format!(
        "---\nname: oca\ndescription: Delegate engineering work to OpenCode workers and inspect or steer their state.\n---\n\n# oca\n\nDispatch every worker with an explicit alias and effort. There is no default.\n\n    {}\n    {}\n\nAliases: luna, sol, terra (gpt-5.6), flash (deepseek-v4-flash-free).\nEfforts: l m h x max. flash accepts h and x only.\n\nUse -b for work that can run while you continue, then immediately park:\n\n    {}\n\nThe follow exits 0 done, 3 blocked, 4 timeout, 5 server unreachable.\n\n    {}\n    {}\n    {}\n    {}\n    {}\n    {}\n\nUse -w when edits must stay isolated. Workers never run git; oca validates the\ndiff and commits locally after each worktree turn.\n\nThe impl reply is status, files, and a note of at most five sentences. A blocked\nworker ends its turn with a question — answer it with `oca m <ref> \"<answer>\"`.\n\n`oca push` and `oca pr` work only where the repository publish grant allows it.\nMerges and grants stay with the human.\n",
        COMMANDS[0],
        COMMANDS[1],
        COMMANDS[5],
        COMMANDS[2],
        COMMANDS[3],
        COMMANDS[4],
        COMMANDS[6],
        COMMANDS[7],
        COMMANDS[8],
    )
}

fn artifacts() -> [(&'static str, String); 3] {
    [
        ("skills/oca/SKILL.md", render_skill()),
        ("templates/opencode-plugin.js", render_plugin().to_owned()),
        ("templates/hook.sh", render_hook().to_owned()),
    ]
}

pub(crate) fn write_artifacts(workspace_root: &Path) -> Result<(), String> {
    for (relative_path, contents) in artifacts() {
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
    let mut drifted = Vec::new();
    for (relative_path, contents) in artifacts() {
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
        COMMANDS, Notification, classify_notification, render_hook, render_plugin, render_skill,
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
        let hook = render_hook();

        assert!(hook.starts_with("#!/bin/sh\n"));
        assert!(hook.contains("count=$(oca ls --blocked --count)"));
        assert!(hook.contains("prev=$(cat \"$state_file\" 2>/dev/null || echo 0)"));
        assert!(hook.contains("[ \"$count\" = \"$prev\" ] && exit 0"));
        assert!(hook.contains(
            "printf 'oca inbox blocked=%s delta=%+d refs=%s\\n' \"$count\" \"$((count-prev))\" \"$(oca ls --blocked --count --refs)\""
        ));
        assert!(hook.ends_with("echo \"$count\" > \"$state_file\"\n"));
    }

    #[test]
    fn skill_and_command_array_publish_only_the_agent_surface() {
        assert_eq!(
            COMMANDS,
            [
                "oca luna:h \"<task>\"",
                "oca flash:h -r impl -w -b \"<task>\"",
                "oca m <ref> \"<message>\"",
                "oca s <ref> \"<message>\"",
                "oca q <ref> \"<message>\"",
                "oca f <ref>",
                "oca k <ref>",
                "oca ls",
                "oca events <ref>",
                "oca push <ref>",
                "oca pr <ref>",
            ]
        );

        let skill = render_skill();
        assert!(skill.starts_with("---\nname: oca\ndescription: Delegate engineering work"));
        assert!(
            skill.contains("Aliases: luna, sol, terra (gpt-5.6), flash (deepseek-v4-flash-free).")
        );
        assert!(
            skill.contains("The follow exits 0 done, 3 blocked, 4 timeout, 5 server unreachable.")
        );
        assert!(skill.contains("Merges and grants stay with the human."));
        for forbidden in ["oca __attach", "oca clean", "oca ping"] {
            assert!(!skill.contains(forbidden), "skill must exclude {forbidden}");
            assert!(COMMANDS.iter().all(|command| !command.contains(forbidden)));
        }
    }
}
