// Seams under test: `RolePreamble`, `render_role_preambles`, and
// `STYLE_EXEMPTION`. The renderer is the one guidance source for all roles.
use xtask::{RolePreamble, STYLE_EXEMPTION, render_role_preambles};

#[test]
fn every_generated_preamble_has_the_ordered_clauses_and_verbatim_exemption() {
    let roles = [
        RolePreamble::new("impl", "the worker cwd", ["status", "files", "note"]),
        RolePreamble::new("review", "the worker cwd", ["status", "findings", "note"]),
        RolePreamble::new(
            "security-audit",
            "the assigned worktree root",
            ["status", "evidence", "note"],
        ),
    ];

    let preambles = render_role_preambles(&roles);

    assert_eq!(preambles.len(), roles.len());
    for role in &roles {
        let preamble = preambles
            .get(role.name())
            .expect("every supplied role has a generated preamble");

        assert!(preamble.contains(STYLE_EXEMPTION));
        assert!(
            preamble.contains("Converse normally in prose so your work is visible in the TUI.")
        );
        assert!(preamble.contains("END your final message with exactly one"));
        assert!(preamble.contains(
            "Use the literal opening and closing fence lines shown here:\n```json\n<the contract JSON>\n```\n"
        ));
        assert!(preamble.contains("Do not place the contract JSON anywhere else"));
        assert_clause_order(preamble);
        assert!(preamble.contains(role.name()));
        for field in role.fields() {
            assert!(preamble.contains(field));
        }
    }

    let impl_preamble = preambles.get("impl").expect("built-in impl preamble");
    assert!(impl_preamble.contains("five-sentence cap"));
}

fn assert_clause_order(preamble: &str) {
    let scope = preamble.find("## Scope").expect("scope clause");
    let denials = preamble.find("## Denials").expect("denials clause");
    let contract = preamble
        .find("## Reply contract")
        .expect("reply-contract clause");
    let exemption = preamble
        .find("## Style exemption")
        .expect("style-exemption clause");

    assert!(scope < denials && denials < contract && contract < exemption);
}
