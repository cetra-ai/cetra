//! Limits, tokenization, positioned errors, and denied function list for caller-written filter and SQL text.

use sqlparser::dialect::GenericDialect;
use sqlparser::keywords::Keyword;
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::{Span, Token, TokenWithSpan, Tokenizer, TokenizerError};

use crate::error::{InvalidQuery, Position};

static DIALECT: GenericDialect = GenericDialect {};

/// Parser recursion limit. A left-deep operator chain is bounded separately (D95).
pub const PARSER_RECURSION_LIMIT: usize = 50;

/// Scalar functions that are denied to prevent unbounded allocations or internal metadata leaks (D-A2-2).
pub const DENIED_FUNCTIONS: &[&str] = &[
    "range",
    "generate_series",
    "array_repeat",
    "list_repeat",
    "repeat",
    "lpad",
    "rpad",
    "array_resize",
    "list_resize",
    "replace",
    "regexp_replace",
    "array_replace",
    "list_replace",
    "array_replace_n",
    "list_replace_n",
    "array_replace_all",
    "list_replace_all",
    "levenshtein",
    "input_file_name",
    "file_row_index",
];

/// Byte and operator-token caps for one kind of caller text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextLimits {
    /// Maximum text length in UTF-8 bytes.
    pub max_bytes: usize,
    /// Maximum number of operator tokens allowed.
    pub max_operators: usize,
}

/// Limits enforced on caller-written Filter DSL strings (4 KiB, 128 operators).
pub const FILTER_LIMITS: TextLimits = TextLimits {
    max_bytes: 4 * 1024,
    max_operators: 128,
};

/// Limits enforced on caller-written SQL console queries (64 KiB, 512 operators).
pub const SQL_LIMITS: TextLimits = TextLimits {
    max_bytes: 64 * 1024,
    max_operators: 512,
};

/// Which caller text a limit or error message is talking about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextKind {
    /// A filter expression.
    Filter,
    /// A SQL console statement.
    Sql,
}

impl TextKind {
    fn noun(self) -> &'static str {
        match self {
            Self::Filter => "filter",
            Self::Sql => "query",
        }
    }
}

/// Tokenizes caller text, rejecting oversized text and operator chains.
pub fn tokenize(
    text: &str,
    limits: &TextLimits,
    kind: TextKind,
) -> Result<Vec<TokenWithSpan>, InvalidQuery> {
    if text.len() > limits.max_bytes {
        return Err(InvalidQuery::new(
            format!(
                "the {} is longer than {} bytes",
                kind.noun(),
                limits.max_bytes
            ),
            None,
        ));
    }
    let mut tokenizer = Tokenizer::new(&DIALECT, text);
    let tokens = tokenizer
        .tokenize_with_location()
        .map_err(tokenizer_failure)?;
    let mut operators = 0;
    for token in &tokens {
        if !is_operator(&token.token) {
            continue;
        }
        operators += 1;
        if operators > limits.max_operators {
            return Err(InvalidQuery::new(
                format!(
                    "the {} has more than {} operators",
                    kind.noun(),
                    limits.max_operators
                ),
                Some(position_of(token.span.start.line, token.span.start.column)),
            ));
        }
    }
    Ok(tokens)
}

/// Builds a `sqlparser` [`Parser`] over tokens already checked by [`tokenize`].
pub fn parser(tokens: Vec<TokenWithSpan>) -> Parser<'static> {
    Parser::new(&DIALECT)
        .with_tokens_with_locations(tokens)
        .with_recursion_limit(PARSER_RECURSION_LIMIT)
}

/// Turns a `sqlparser` parse error into a positioned [`InvalidQuery`].
pub fn parse_failure(error: ParserError, parser: &Parser<'_>, text: &str) -> InvalidQuery {
    let (message, located) = match error {
        ParserError::RecursionLimitExceeded => {
            ("the expression is nested too deeply".to_owned(), None)
        }
        ParserError::ParserError(message) | ParserError::TokenizerError(message) => {
            split_appended_location(&message)
        }
    };
    let position = located.or_else(|| {
        let token = parser.peek_token_ref();
        if matches!(token.token, Token::EOF) || token.span == Span::empty() {
            Some(end_of(text))
        } else if token.span.start.line == 0 {
            None
        } else {
            Some(position_of(token.span.start.line, token.span.start.column))
        }
    });
    InvalidQuery::new(message, position)
}

/// Fails when the parser has not consumed the full expression.
pub fn expect_end(parser: &mut Parser<'_>, _text: &str) -> Result<(), InvalidQuery> {
    let token = parser.peek_token_ref();
    if matches!(token.token, Token::EOF) {
        Ok(())
    } else {
        Err(InvalidQuery::new(
            "unexpected text after the expression",
            Some(position_of(token.span.start.line, token.span.start.column)),
        ))
    }
}

/// Converts a [`TokenizerError`] into a positioned [`InvalidQuery`].
pub fn tokenizer_failure(error: TokenizerError) -> InvalidQuery {
    InvalidQuery::new(
        error.message,
        Some(position_of(error.location.line, error.location.column)),
    )
}

/// Converts 1-based `u64` line/column values to [`Position`], saturating at `u32::MAX`.
pub fn position_of(line: u64, column: u64) -> Position {
    Position {
        line: u32::try_from(line).unwrap_or(u32::MAX),
        column: u32::try_from(column).unwrap_or(u32::MAX),
    }
}

/// Computes the 1-based position immediately past the end of `text`.
pub fn end_of(text: &str) -> Position {
    if text.is_empty() {
        return Position { line: 1, column: 1 };
    }
    let mut line = 1u32;
    let mut column = 1u32;
    let mut last_line = 1u32;
    let mut last_column = 1u32;
    for ch in text.chars() {
        last_line = line;
        last_column = column;
        if ch == '\n' {
            line = line.saturating_add(1);
            column = 1;
        } else {
            column = column.saturating_add(1);
        }
    }
    Position {
        line: last_line,
        column: last_column.saturating_add(1),
    }
}

/// Splits sqlparser's `"msg at Line: L, Column: C"` suffix into `(msg, Some(Position))`.
pub fn split_appended_location(message: &str) -> (String, Option<Position>) {
    let marker = " at Line: ";
    let Some(index) = message.rfind(marker) else {
        return (message.to_owned(), None);
    };
    let tail = &message[index + marker.len()..];
    let Some((line, column)) = tail.split_once(", Column: ") else {
        return (message.to_owned(), None);
    };
    if line.is_empty()
        || column.is_empty()
        || !line.bytes().all(|byte| byte.is_ascii_digit())
        || !column.bytes().all(|byte| byte.is_ascii_digit())
    {
        return (message.to_owned(), None);
    }
    let line = line.parse::<u64>().unwrap_or(u64::from(u32::MAX));
    let column = column.parse::<u64>().unwrap_or(u64::from(u32::MAX));
    (
        message[..index].trim_end().to_owned(),
        Some(position_of(line, column)),
    )
}

/// Returns true if `token` counts toward the operator budget (45 symbol + 23 keyword tokens).
pub fn is_operator(token: &Token) -> bool {
    match token {
        Token::Eq
        | Token::DoubleEq
        | Token::Neq
        | Token::Lt
        | Token::Gt
        | Token::LtEq
        | Token::GtEq
        | Token::Spaceship
        | Token::Plus
        | Token::Minus
        | Token::Mul
        | Token::Div
        | Token::Mod
        | Token::DuckIntDiv
        | Token::StringConcat
        | Token::AtArrow
        | Token::ArrowAt
        | Token::Arrow
        | Token::LongArrow
        | Token::HashArrow
        | Token::HashLongArrow
        | Token::AtAt
        | Token::Question
        | Token::QuestionAnd
        | Token::QuestionPipe
        | Token::Ampersand
        | Token::Pipe
        | Token::Caret
        | Token::ShiftLeft
        | Token::ShiftRight
        | Token::Tilde
        | Token::TildeAsterisk
        | Token::ExclamationMarkTilde
        | Token::ExclamationMarkTildeAsterisk
        | Token::DoubleColon
        | Token::LBracket
        | Token::Colon
        | Token::Sharp
        | Token::DoubleTilde
        | Token::DoubleTildeAsterisk
        | Token::ExclamationMarkDoubleTilde
        | Token::ExclamationMarkDoubleTildeAsterisk
        | Token::HashMinus
        | Token::AtQuestion
        | Token::CaretAt => true,
        Token::Word(word) => matches!(
            word.keyword,
            Keyword::AND
                | Keyword::OR
                | Keyword::NOT
                | Keyword::IS
                | Keyword::IN
                | Keyword::BETWEEN
                | Keyword::LIKE
                | Keyword::ILIKE
                | Keyword::SIMILAR
                | Keyword::XOR
                | Keyword::DIV
                | Keyword::UNION
                | Keyword::INTERSECT
                | Keyword::EXCEPT
                | Keyword::MINUS
                | Keyword::AT
                | Keyword::COLLATE
                | Keyword::RLIKE
                | Keyword::REGEXP
                | Keyword::MATCH
                | Keyword::MEMBER
                | Keyword::OVERLAPS
                | Keyword::OPERATOR
        ),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use sqlparser::tokenizer::Location;

    use super::*;

    fn parse_err(text: &str) -> InvalidQuery {
        let tokens = tokenize(text, &FILTER_LIMITS, TextKind::Filter).expect("tokens");
        let mut p = parser(tokens);
        let err = p.parse_expr().expect_err(text);
        parse_failure(err, &p, text)
    }

    #[test]
    fn limits_operators_and_positions() {
        // 4 KiB filter limit
        assert!(tokenize(&"a".repeat(4096), &FILTER_LIMITS, TextKind::Filter).is_ok());
        let over_filter =
            tokenize(&"a".repeat(4097), &FILTER_LIMITS, TextKind::Filter).expect_err("over 4 KiB");
        assert_eq!(over_filter.message, "the filter is longer than 4096 bytes");
        assert_eq!(over_filter.position, None);

        // 128 operator filter limit
        let ok_ops = format!("{}a", "a OR ".repeat(128));
        assert!(tokenize(&ok_ops, &FILTER_LIMITS, TextKind::Filter).is_ok());
        let over_ops = format!("{}a", "a OR ".repeat(129));
        let err_ops =
            tokenize(&over_ops, &FILTER_LIMITS, TextKind::Filter).expect_err("over 128 ops");
        assert_eq!(err_ops.message, "the filter has more than 128 operators");
        assert_eq!(
            err_ops.position,
            Some(Position {
                line: 1,
                column: 643
            })
        );

        // 64 KiB / 512 ops SQL limit
        assert!(tokenize(&"a".repeat(65536), &SQL_LIMITS, TextKind::Sql).is_ok());
        let over_sql =
            tokenize(&"a".repeat(65537), &SQL_LIMITS, TextKind::Sql).expect_err("over 64 KiB");
        assert_eq!(over_sql.message, "the query is longer than 65536 bytes");
        assert_eq!(over_sql.position, None);

        let ok_sql_ops = format!("{}a", "a OR ".repeat(512));
        assert!(tokenize(&ok_sql_ops, &SQL_LIMITS, TextKind::Sql).is_ok());
        let over_sql_ops = format!("{}a", "a OR ".repeat(513));
        let err_sql_ops =
            tokenize(&over_sql_ops, &SQL_LIMITS, TextKind::Sql).expect_err("over 512 ops");
        assert_eq!(err_sql_ops.message, "the query has more than 512 operators");
        assert!(err_sql_ops.position.is_some());

        // Quoted keyword "union" = 0 ops; unquoted operator tokens match
        let cases = [
            ("SELECT 1 UNION SELECT 1", 1),
            ("SELECT 1 UNION ALL SELECT 1", 1),
            ("SELECT 1 INTERSECT SELECT 1", 1),
            ("SELECT 1 EXCEPT SELECT 1", 1),
            ("SELECT 1 MINUS SELECT 1", 1),
            ("SELECT now() AT TIME ZONE 'UTC'", 1),
            ("SELECT 'a' COLLATE x", 1),
            ("SELECT metadata['k'] FROM spans", 1),
            ("SELECT 'a' ~~ 'a'", 1),
            ("SELECT 'a' RLIKE 'a'", 1),
            ("SELECT {n:Int}", 1),
            ("SELECT \"union\" FROM spans", 0),
        ];
        for (text, expected) in cases {
            let tokens = Tokenizer::new(&DIALECT, text)
                .tokenize_with_location()
                .expect("tokens");
            let counted = tokens.iter().filter(|t| is_operator(&t.token)).count();
            assert_eq!(counted, expected, "{text}");
        }

        // Recursion limit 50
        assert_eq!(PARSER_RECURSION_LIMIT, 50);
        let deep = format!("{}true{}", "(".repeat(60), ")".repeat(60));
        let deep_err = parse_err(&deep);
        assert_eq!(deep_err.message, "the expression is nested too deeply");

        // expect_end
        let trailing = "1 2";
        let tokens = tokenize(trailing, &SQL_LIMITS, TextKind::Sql).expect("tokens");
        let mut p = parser(tokens);
        p.parse_expr().expect("one expr");
        let end_err = expect_end(&mut p, trailing).expect_err("trailing");
        assert_eq!(end_err.message, "unexpected text after the expression");
        assert_eq!(end_err.position, Some(Position { line: 1, column: 3 }));

        // split_appended_location
        let (msg, pos) = split_appended_location("Expected: expr, found: = at Line: 2, Column: 7");
        assert_eq!(msg, "Expected: expr, found: =");
        assert_eq!(pos, Some(Position { line: 2, column: 7 }));
        let (msg_no, pos_no) = split_appended_location("plain error without location");
        assert_eq!(msg_no, "plain error without location");
        assert_eq!(pos_no, None);

        // tokenizer_failure and 1-based Position
        let tok_err = tokenizer_failure(TokenizerError {
            message: "Unterminated string literal".to_owned(),
            location: Location { line: 3, column: 9 },
        });
        assert_eq!(tok_err.message, "Unterminated string literal");
        assert_eq!(tok_err.position, Some(Position { line: 3, column: 9 }));

        let unterminated = tokenize(
            "status = 'é' AND name = 'oops",
            &FILTER_LIMITS,
            TextKind::Filter,
        )
        .expect_err("unterminated");
        assert_eq!(
            unterminated.position,
            Some(Position {
                line: 1,
                column: 25
            })
        );

        // 20 denied functions
        assert_eq!(DENIED_FUNCTIONS.len(), 20);
    }
}
