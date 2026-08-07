//! Deterministic display names for headed worker dispatches.

const IGNORED_WORDS: &[&str] = &[
    "a", "an", "and", "for", "in", "of", "on", "or", "please", "the", "then", "to", "with",
];

/// Derives a short lower-camel-case display name from a dispatch prompt.
///
/// At most the first two useful ASCII words are retained. The ref is returned
/// when the prompt has no usable word, keeping every headed dispatch labelable
/// without a model call.
#[must_use]
pub fn task_display_name(prompt: &str, reference: &str) -> String {
    let words = prompt
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|word| word.bytes().any(|byte| byte.is_ascii_alphabetic()))
        .map(str::to_ascii_lowercase)
        .filter(|word| !IGNORED_WORDS.contains(&word.as_str()))
        .take(2)
        .collect::<Vec<_>>();

    let Some((first, rest)) = words.split_first() else {
        return reference.to_owned();
    };

    let mut name = first.clone();
    for word in rest {
        let mut characters = word.chars();
        if let Some(initial) = characters.next() {
            name.push(initial.to_ascii_uppercase());
            name.extend(characters);
        }
    }
    name
}

#[cfg(test)]
mod tests {
    use super::task_display_name;

    #[test]
    fn lower_camel_cases_the_first_two_useful_words() {
        assert_eq!(task_display_name("Say hi to the team", "wabc12"), "sayHi");
        assert_eq!(task_display_name("FIX PARSER NOW", "wabc12"), "fixParser");
    }

    #[test]
    fn skips_joining_words_and_caps_the_name_at_two_words() {
        assert_eq!(
            task_display_name("Create a file named artifact.txt", "wabc12"),
            "createFile"
        );
        assert_eq!(
            task_display_name("Please repair the request router today", "wabc12"),
            "repairRequest"
        );
    }

    #[test]
    fn falls_back_to_the_ref_without_a_usable_ascii_word() {
        for prompt in ["", "... 1234 !!!", "please, the, and", "🚀 你好"] {
            assert_eq!(task_display_name(prompt, "wabc12"), "wabc12");
        }
    }
}
