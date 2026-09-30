//! Claude composer가 선택할 수 있는 이름과 검색 키를 공통으로 관리한다.

use anyhow::Result;
use anyhow::ensure;
use unicode_general_category::GeneralCategory;
use unicode_general_category::get_general_category;
use unicode_normalization::UnicodeNormalization;

fn invisible(c: char) -> bool {
    matches!(
        get_general_category(c),
        GeneralCategory::Control | GeneralCategory::Format
    )
}

pub fn normalize_name(name: &str) -> String {
    name.nfkc()
        .filter(|c| !invisible(*c))
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("-")
}

pub fn validate_name(name: &str) -> Result<()> {
    let plain = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'));
    let lower = normalize_name(name);
    let has_reference = name.match_indices('[').any(|(offset, _)| {
        name[offset + 1..]
            .split_once(']')
            .is_some_and(|(value, _)| {
                (6..=12).contains(&value.len())
                    && value.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
    });
    ensure!(
        !name.is_empty()
            && name.trim() == name
            && name.chars().count() <= if plain { 128 } else { 200 }
            && !name.chars().any(|c| invisible(c) || "\"<>@*".contains(c))
            && !lower.starts_with("agent-")
            && !lower.ends_with("(agent)")
            && !lower.starts_with("uds:")
            && !lower.starts_with("bridge:")
            && !lower.starts_with("did:")
            && !(lower.starts_with('/') && lower.ends_with(".sock"))
            && !matches!(lower.as_str(), "main" | "team-lead" | "user" | "system")
            && !has_reference,
        "peer name is not valid for Claude session mentions"
    );
    Ok(())
}

pub fn mention(name: &str) -> String {
    if name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
    {
        format!("@{name}")
    } else {
        format!("@{}", serde_json::to_string(name).unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_names_support_renamed_unicode_sessions() {
        assert!(validate_name("claudex 업데이트").is_ok());
        assert!(validate_name(&"가".repeat(100)).is_ok());
        assert_eq!(mention("claudex 업데이트"), "@\"claudex 업데이트\"");
        assert_eq!(mention("CSM_TEST"), "@CSM_TEST");
        assert_eq!(
            normalize_name("ＣＬＡＵＤＥＸ 업데이트"),
            "claudex-업데이트"
        );
    }

    #[test]
    fn peer_names_reject_ambiguous_mentions() {
        for name in [
            "",
            " name",
            "x\ny",
            "@name",
            "x*y",
            "a\"b",
            "<name>",
            "agent-x",
            "x(agent)",
            "uds:/tmp/x",
            "bridge:x",
            "x [abcdef]",
            "[abcdef] name",
            "main",
            "TEAM-LEAD",
            "did:name",
            "x\u{200b}y",
        ] {
            assert!(validate_name(name).is_err(), "{name:?}");
        }
        assert!(validate_name(&"x".repeat(129)).is_err());
        assert!(validate_name(&"가".repeat(201)).is_err());
    }
}
