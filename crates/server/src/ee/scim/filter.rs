// SPDX-License-Identifier: Elastic-2.0
//! The SCIM filter parser for `/Users` (RFC 7644 § 3.4.2.2, track EA M5, D-EA-12). Only one `eq`
//! on `userName`, `externalId` or the work email is accepted. Every other filter is
//! `400 invalidFilter`.

use std::str::Chars;

use super::error::ScimError;
use super::store::{GroupFilter, UserFilter};

/// The longest accepted value, in characters.
const MAX_VALUE_CHARS: usize = 320;

/// The attribute of a parsed filter.
enum Attribute {
    UserName,
    ExternalId,
    Email,
}

/// Parses a `/Users` filter such as `userName eq "ada@example.com"`.
///
/// # Errors
///
/// Returns `invalidFilter` for anything that is not one `eq` with a quoted JSON string on
/// `userName`, `externalId`, `emails.value` or `emails[type eq "work"].value`.
pub(crate) fn parse_user_filter(input: &str) -> Result<UserFilter, ScimError> {
    let (attribute, rest) = parse_attribute(input.trim())?;
    let rest = spaces(rest)?;
    let rest = strip_prefix_ci(rest, "eq").ok_or_else(ScimError::invalid_filter)?;
    let value = parse_comp_value(spaces(rest)?)?;
    Ok(match attribute {
        Attribute::UserName => UserFilter::UserName(value),
        Attribute::ExternalId => UserFilter::ExternalId(value),
        Attribute::Email => UserFilter::Email(value),
    })
}

/// Parses a `/Groups` filter such as `displayName eq "Engineering"`. Only one `eq` on
/// `displayName` or `externalId` is accepted; member filters are refused (D-EA-12).
///
/// # Errors
///
/// Returns `invalidFilter` with the group wording for anything else.
pub(crate) fn parse_group_filter(input: &str) -> Result<GroupFilter, ScimError> {
    let refused = |_: ScimError| ScimError::invalid_group_filter();
    let input = input.trim();
    let end = input
        .find(' ')
        .ok_or_else(ScimError::invalid_group_filter)?;
    let (name, rest) = input.split_at(end);
    let rest = spaces(rest).map_err(refused)?;
    let rest = strip_prefix_ci(rest, "eq").ok_or_else(ScimError::invalid_group_filter)?;
    let value = parse_comp_value(spaces(rest).map_err(refused)?).map_err(refused)?;
    if name.eq_ignore_ascii_case("displayName") {
        Ok(GroupFilter::DisplayName(value))
    } else if name.eq_ignore_ascii_case("externalId") {
        Ok(GroupFilter::ExternalId(value))
    } else {
        Err(ScimError::invalid_group_filter())
    }
}

/// `input` without the case-insensitive `prefix`, or `None` when it does not start with it.
pub(crate) fn strip_prefix_ci<'a>(input: &'a str, prefix: &str) -> Option<&'a str> {
    let head = input.get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix) {
        input.get(prefix.len()..)
    } else {
        None
    }
}

/// The attribute path and the rest of the filter after it.
fn parse_attribute(input: &str) -> Result<(Attribute, &str), ScimError> {
    if let Some(rest) = strip_prefix_ci(input, "emails[") {
        // The Entra form: emails[type eq "work"].value
        let rest = rest.trim_start_matches(' ');
        let rest = strip_prefix_ci(rest, "type").ok_or_else(ScimError::invalid_filter)?;
        let rest = strip_prefix_ci(spaces(rest)?, "eq").ok_or_else(ScimError::invalid_filter)?;
        let rest = spaces(rest)?;
        let rest = rest
            .strip_prefix("\"work\"")
            .ok_or_else(ScimError::invalid_filter)?;
        let rest = strip_prefix_ci(rest.trim_start_matches(' '), "]")
            .ok_or_else(ScimError::invalid_filter)?;
        let rest = strip_prefix_ci(rest, ".value").ok_or_else(ScimError::invalid_filter)?;
        return Ok((Attribute::Email, rest));
    }
    // The rest keeps its leading space: `parse_user_filter` requires one before the operator.
    let end = input.find(' ').ok_or_else(ScimError::invalid_filter)?;
    let (name, rest) = input.split_at(end);
    let attribute = if name.eq_ignore_ascii_case("userName") {
        Attribute::UserName
    } else if name.eq_ignore_ascii_case("externalId") {
        Attribute::ExternalId
    } else if name.eq_ignore_ascii_case("emails.value") {
        Attribute::Email
    } else {
        return Err(ScimError::invalid_filter());
    };
    Ok((attribute, rest))
}

/// `input` after one or more spaces. Extra spaces are accepted; no space is an invalid filter.
pub(crate) fn spaces(input: &str) -> Result<&str, ScimError> {
    let rest = input.trim_start_matches(' ');
    if rest.len() == input.len() {
        Err(ScimError::invalid_filter())
    } else {
        Ok(rest)
    }
}

/// A quoted JSON string that is the whole of `input`. Escapes follow JSON.
pub(crate) fn parse_comp_value(input: &str) -> Result<String, ScimError> {
    let body = input
        .strip_prefix('"')
        .ok_or_else(ScimError::invalid_filter)?;
    let mut chars = body.chars();
    let mut value = String::new();
    let mut length = 0usize;
    loop {
        let c = chars.next().ok_or_else(ScimError::invalid_filter)?;
        let decoded = match c {
            '"' => break,
            '\\' => decode_escape(&mut chars)?,
            c if c < ' ' => return Err(ScimError::invalid_filter()),
            c => c,
        };
        length += 1;
        if length > MAX_VALUE_CHARS {
            return Err(ScimError::invalid_filter());
        }
        value.push(decoded);
    }
    if chars.next().is_some() {
        return Err(ScimError::invalid_filter());
    }
    Ok(value)
}

/// The character of one escape, after its backslash.
fn decode_escape(chars: &mut Chars<'_>) -> Result<char, ScimError> {
    let escaped = chars.next().ok_or_else(ScimError::invalid_filter)?;
    Ok(match escaped {
        '"' => '"',
        '\\' => '\\',
        '/' => '/',
        'b' => '\u{8}',
        'f' => '\u{c}',
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        'u' => {
            let unit = hex4(chars)?;
            if (0xD800..=0xDBFF).contains(&unit) {
                // A high surrogate must be followed by an escaped low surrogate.
                if chars.next() != Some('\\') || chars.next() != Some('u') {
                    return Err(ScimError::invalid_filter());
                }
                let low = hex4(chars)?;
                if !(0xDC00..=0xDFFF).contains(&low) {
                    return Err(ScimError::invalid_filter());
                }
                let code = 0x1_0000 + ((unit - 0xD800) << 10) + (low - 0xDC00);
                char::from_u32(code).ok_or_else(ScimError::invalid_filter)?
            } else {
                char::from_u32(unit).ok_or_else(ScimError::invalid_filter)?
            }
        }
        _ => return Err(ScimError::invalid_filter()),
    })
}

/// Four hex digits of a `\u` escape.
fn hex4(chars: &mut Chars<'_>) -> Result<u32, ScimError> {
    let mut value = 0;
    for _ in 0..4 {
        let digit = chars
            .next()
            .and_then(|c| c.to_digit(16))
            .ok_or_else(ScimError::invalid_filter)?;
        value = value * 16 + digit;
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use rand::rngs::StdRng;
    use rand::{RngExt, SeedableRng};

    use super::*;

    /// The filter's kind and value, or `None` when it is refused.
    fn kind_and_value(input: &str) -> Option<(&'static str, String)> {
        match parse_user_filter(input).ok()? {
            UserFilter::UserName(value) => Some(("userName", value)),
            UserFilter::ExternalId(value) => Some(("externalId", value)),
            UserFilter::Email(value) => Some(("email", value)),
        }
    }

    #[test]
    fn parses_user_name_eq() {
        assert_eq!(
            kind_and_value(r#"userName eq "ada@example.com""#),
            Some(("userName", "ada@example.com".to_owned()))
        );
    }

    #[test]
    fn parses_external_id_eq() {
        assert_eq!(
            kind_and_value(r#"externalId eq "okta-00u1""#),
            Some(("externalId", "okta-00u1".to_owned()))
        );
    }

    #[test]
    fn parses_emails_value_and_work_filter() {
        assert_eq!(
            kind_and_value(r#"emails.value eq "ada@example.com""#),
            Some(("email", "ada@example.com".to_owned()))
        );
        assert_eq!(
            kind_and_value(r#"emails[type eq "work"].value eq "ada@example.com""#),
            Some(("email", "ada@example.com".to_owned()))
        );
    }

    #[test]
    fn attribute_names_are_case_insensitive() {
        assert_eq!(
            kind_and_value(r#"USERNAME EQ "a@x.io""#),
            Some(("userName", "a@x.io".to_owned()))
        );
        assert_eq!(
            kind_and_value(r#"externalid eq "x""#),
            Some(("externalId", "x".to_owned()))
        );
        assert_eq!(
            kind_and_value(r#"EMAILS.VALUE eq "a@x.io""#),
            Some(("email", "a@x.io".to_owned()))
        );
        assert_eq!(
            kind_and_value(r#"Emails[Type Eq "work"].Value eq "a@x.io""#),
            Some(("email", "a@x.io".to_owned()))
        );
    }

    #[test]
    fn escaped_quotes_in_values() {
        assert_eq!(
            kind_and_value(r#"userName eq "a\"b""#),
            Some(("userName", "a\"b".to_owned()))
        );
        assert_eq!(
            kind_and_value(r#"externalId eq "back\\slash \/ é 😀""#),
            Some(("externalId", "back\\slash / \u{e9} \u{1F600}".to_owned()))
        );
    }

    #[test]
    fn surrounding_and_extra_spaces_are_accepted() {
        assert_eq!(
            kind_and_value(r#"   userName   eq    "a@x.io"   "#),
            Some(("userName", "a@x.io".to_owned()))
        );
    }

    #[test]
    fn refuses_other_operators() {
        assert!(parse_user_filter(r#"userName co "a""#).is_err());
        assert!(parse_user_filter(r#"userName sw "a""#).is_err());
        assert!(parse_user_filter("externalId pr").is_err());
        assert!(parse_user_filter(r#"userName gt "a""#).is_err());
    }

    #[test]
    fn refuses_and_or_not() {
        assert!(parse_user_filter(r#"userName eq "a" and externalId eq "b""#).is_err());
        assert!(parse_user_filter(r#"userName eq "a" or externalId eq "b""#).is_err());
        assert!(parse_user_filter(r#"not (userName eq "a")"#).is_err());
    }

    #[test]
    fn refuses_unknown_attributes() {
        assert!(parse_user_filter(r#"title eq "x""#).is_err());
        assert!(parse_user_filter(r#"name.givenName eq "x""#).is_err());
        assert!(parse_user_filter(r#"emails[type eq "home"].value eq "x""#).is_err());
    }

    #[test]
    fn refuses_unquoted_values() {
        assert!(parse_user_filter("userName eq ada").is_err());
        assert!(parse_user_filter("userName eq 1").is_err());
    }

    #[test]
    fn refuses_trailing_garbage() {
        assert!(parse_user_filter(r#"userName eq "a" x"#).is_err());
        assert!(parse_user_filter(r#"userName eq "a"""#).is_err());
        assert!(parse_user_filter(r#"userName eq "a"b"#).is_err());
    }

    #[test]
    fn refuses_empty_and_unterminated_input() {
        assert!(parse_user_filter("").is_err());
        assert!(parse_user_filter("userName").is_err());
        assert!(parse_user_filter(r#"userName eq "abc"#).is_err());
        assert!(parse_user_filter(r#"userName eq "a\"#).is_err());
        assert!(parse_user_filter(r#"userName eq "a\u12""#).is_err());
        assert!(parse_user_filter(r#"userName eq "\ud83d""#).is_err());
    }

    #[test]
    fn values_longer_than_320_characters_are_refused() {
        let long = "a".repeat(MAX_VALUE_CHARS);
        assert!(parse_user_filter(&format!(r#"userName eq "{long}""#)).is_ok());
        let too_long = "a".repeat(MAX_VALUE_CHARS + 1);
        assert!(parse_user_filter(&format!(r#"userName eq "{too_long}""#)).is_err());
    }

    /// The group filter's kind and value, or `None` when it is refused.
    fn group_kind_and_value(input: &str) -> Option<(&'static str, String)> {
        match parse_group_filter(input).ok()? {
            GroupFilter::DisplayName(value) => Some(("displayName", value)),
            GroupFilter::ExternalId(value) => Some(("externalId", value)),
        }
    }

    #[test]
    fn parses_display_name_eq() {
        assert_eq!(
            group_kind_and_value(r#"displayName eq "Engineering""#),
            Some(("displayName", "Engineering".to_owned()))
        );
        assert_eq!(
            group_kind_and_value(r#"DISPLAYNAME   EQ "Eng \"A\"""#),
            Some(("displayName", "Eng \"A\"".to_owned()))
        );
        assert_eq!(
            group_kind_and_value(r#"externalId eq "okta-00g1""#),
            Some(("externalId", "okta-00g1".to_owned()))
        );
    }

    #[test]
    fn refuses_member_filters_on_groups() {
        let long = "a".repeat(MAX_VALUE_CHARS + 1);
        let too_long = format!(r#"displayName eq "{long}""#);
        for input in [
            r#"members[value eq "u1"]"#,
            r#"members eq "u1""#,
            r#"emails.value eq "a@x.io""#,
            r#"userName eq "a@x.io""#,
            r#"displayName co "Eng""#,
            r#"displayName eq "a" and externalId eq "b""#,
            r#"displayName eq Eng"#,
            r#"displayName eq "abc"#,
            "displayName",
            "",
            too_long.as_str(),
        ] {
            assert!(parse_group_filter(input).is_err(), "refused: {input:.40}");
        }
    }

    #[test]
    fn the_parser_never_panics() {
        const CHARS: &[u8] = b"userName eq \"x\" []().\\";
        let mut rng = StdRng::seed_from_u64(7);
        for _ in 0..2000 {
            let length = rng.random_range(0..=80usize);
            let input: String = (0..length)
                .map(|_| char::from(CHARS[rng.random_range(0..CHARS.len())]))
                .collect();
            let _ = parse_user_filter(&input);
        }
    }
}
