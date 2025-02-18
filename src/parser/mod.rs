//! Cooklang parser
//!
//! Grammar:
//! ```txt
//! recipe     = Newline* (line line_end)* line? Eof
//! line       = metadata | section | step
//! line_end   = soft_break | Newline+
//! soft_break = Newline !Newline
//!
//! metadata   = MetadataStart meta_key Colon meta_val
//! meta_key   = (!(Colon | Newline) ANY)*
//! meta_value = (!Newline ANY)*
//!
//! section    = Eq+ (section_name Eq*)
//! sect_name  = (!Eq ANY)*
//!
//! step       = TextStep? (component | ANY)*
//!
//! component  = c_kind modifiers? c_body note?
//! c_kind     = At | Hash | Tilde
//! c_body     = c_close | c_long | Word
//! c_long     = c_l_name c_alias? c_close
//! c_l_name   = (!(Newline | OpenBrace | Or) ANY)*
//! c_alias    = Or c_l_name
//! c_close    = OpenBrace Whitespace? Quantity? Whitespace? CloseBrace
//!
//! modifiers  = modifier+
//! modifier   = (At (OpenParen Eq? Tilde? Int CloseParen)?) | And | Plus | Minus | Question
//!
//! note       = OpenParen (!CloseParen ANY)* CloseParen
//!
//! quantity   = num_val Whitespace !(unit_sep | auto_scale | val_sep) unit
//!            | val (val_sep val)* auto_scale? (unit_sep unit)?
//!
//! unit       = (!CloseBrace ANY)*
//!
//! val_sep    = Whitespace Or Whitespace
//! auto_scale = Whitespace Star Whitespace
//! unit_sep   = Whitespace Percent Whitespace
//!
//! val        = num_val | text_val
//! text_val   = (Word | Whitespace)*
//! num_val    = mixed_num | frac | range | num
//! mixed_num  = Int Whitespace frac
//! frac       = Int Whitespace Slash Whitespace Int
//! range      = num Whitespace Minus Whitespace Num
//! num        = Float | Int
//!
//!
//! ANY        = { Any token }
//! ```
//! This is more of a guideline, there may be edge cases that this grammar does
//! not cover but the pareser does.

mod metadata;
mod quantity;
mod section;
mod step;
mod token_stream;

use std::borrow::Cow;

use thiserror::Error;

use crate::{
    ast,
    context::Context,
    error::{PassResult, RichError},
    lexer::T,
    located::Located,
    parser::{metadata::metadata_entry, section::section, step::step},
    span::Span,
    Extensions,
};

use token_stream::{Token, TokenKind, TokenStream};

#[derive(Debug)]
pub(crate) struct Parser<'input, T>
where
    T: Iterator<Item = Token>,
{
    input: &'input str,
    tokens: T,
    line: Vec<Token>,
    offset: usize,

    /// Error and warning context
    pub(crate) context: Context<ParserError, ParserWarning>,
    /// Extensions to cooklang language
    pub(crate) extensions: Extensions,
}

impl<'input> Parser<'input, TokenStream<'input>> {
    pub fn new(input: &'input str, extensions: Extensions) -> Self {
        Self::new_from_token_iter(input, extensions, TokenStream::new(input))
    }
}

impl<'input, I> Parser<'input, I>
where
    I: Iterator<Item = Token>,
{
    pub fn new_from_token_iter(input: &'input str, extensions: Extensions, tokens: I) -> Self {
        Self {
            input,
            tokens,
            line: Vec::new(),
            context: Context::default(),
            extensions,
            offset: 0,
        }
    }
}

impl<'input, I> Parser<'input, I>
where
    I: Iterator<Item = Token>,
{
    /// Advances a line. Store the tokens, newline/eof excluded.
    pub(crate) fn next_line(&mut self) -> Option<LineParser<'_, 'input>> {
        self.line.clear();
        let parsed = self.offset;
        let mut has_terminator = false;
        for token in self.tokens.by_ref() {
            self.offset += token.len();
            if matches!(token.kind, T![newline] | T![eof]) {
                has_terminator = true;
                break;
            }
            self.line.push(token);
        }
        if self.line.is_empty() && !has_terminator {
            None
        } else {
            Some(LineParser::new(
                parsed,
                &self.line,
                self.input,
                self.extensions,
            ))
        }
    }
}

/// Parse a recipe into an [`Ast`](ast::Ast)
#[tracing::instrument(level = "debug", skip_all, fields(len = input.len()))]
pub fn parse<'input>(
    input: &'input str,
    extensions: Extensions,
) -> PassResult<ast::Ast<'input>, ParserError, ParserWarning> {
    let mut parser = Parser::new(input, extensions);

    let mut last_empty = true;
    let mut lines = Vec::new();
    while let Some(mut line) = parser.next_line() {
        parse_line(&mut line, &mut lines, &mut last_empty);
        let mut ctx = line.finish();
        parser.context.append(&mut ctx);
    }

    let ast = ast::Ast { lines };
    parser.context.finish(Some(ast))
}

fn parse_line<'input>(
    line: &mut LineParser<'_, 'input>,
    lines: &mut Vec<ast::Line<'input>>,
    last_empty: &mut bool,
) {
    let is_empty = line
        .tokens()
        .iter()
        .all(|t| matches!(t.kind, T![ws] | T![line comment] | T![block comment]));
    if is_empty {
        *last_empty = true;
        line.consume_rest();
        return;
    }

    let meta_or_section = match line.peek() {
        T![meta] => line
            .with_recover(metadata_entry)
            .map(|entry| ast::Line::Metadata {
                key: entry.key,
                value: entry.value,
            }),
        T![=] => line
            .with_recover(section)
            .map(|name| ast::Line::Section { name }),
        _ => None,
    };

    let ast_line = if let Some(l) = meta_or_section {
        l
    } else {
        if !*last_empty && line.extension(Extensions::MULTILINE_STEPS) {
            if let Some(ast::Line::Step { items, is_text }) = lines.last_mut() {
                let mut parsed_step = step(line, *is_text);
                if !parsed_step.items.is_empty() {
                    // pos of the newline/end of last step before trimming
                    let newline_pos = items.last().unwrap().span().end();
                    // trim last step end
                    if let Some(ast::Item::Text(text)) = items.last_mut() {
                        text.trim_fragments_end();
                        if text.fragments().is_empty() {
                            items.pop();
                        }
                    }
                    // trim new step begining
                    if let ast::Item::Text(text) = &mut parsed_step.items[0] {
                        text.trim_fragments_start();
                        if text.fragments().is_empty() {
                            parsed_step.items.remove(0);
                        }
                    }
                    // add a space in between the 2 lines
                    // where the last line originally ended in the input
                    items.push(ast::Item::Text(ast::Text::from_str(" ", newline_pos)));
                    items.extend(parsed_step.items);
                }
                return;
            }
        }

        let parsed_step = step(line, false);
        ast::Line::Step {
            is_text: parsed_step.is_text,
            items: parsed_step.items,
        }
    };

    *last_empty = false;
    lines.push(ast_line);
}

/// Parse only the recipe metadata into an [`Ast`](ast::Ast).
///
/// This will skip every line that is not metadata. Is faster than [`parse`].
#[tracing::instrument(level = "debug", skip_all, fields(len = input.len()))]
pub fn parse_metadata<'input>(
    input: &'input str,
) -> PassResult<ast::Ast<'input>, ParserError, ParserWarning> {
    let mut parser = Parser::new(input, Extensions::empty());
    let mut lines = vec![];
    while let Some(mut line) = parser.next_line() {
        let meta_line = match line.peek() {
            T![meta] => line
                .with_recover(metadata_entry)
                .map(|entry| ast::Line::Metadata {
                    key: entry.key,
                    value: entry.value,
                }),
            _ => {
                line.consume_rest();
                continue;
            }
        };
        if let Some(meta_line) = meta_line {
            lines.push(meta_line);
        }
    }
    let ast = ast::Ast { lines };
    parser.context.finish(Some(ast))
}

pub(crate) struct LineParser<'t, 'input> {
    base_offset: usize,
    tokens: &'t [Token],
    current: usize,
    pub(crate) input: &'input str,
    pub(crate) context: Context<ParserError, ParserWarning>,
    pub(crate) extensions: Extensions,
}

impl<'t, 'input> LineParser<'t, 'input> {
    /// Create it from separate parts.
    /// - tokens must be adjacent (checked in debug)
    /// - slices's tokens's span must refer to the input (checked in debug)
    /// - input is the whole input str given to the lexer
    pub(crate) fn new(
        base_offset: usize,
        line: &'t [Token],
        input: &'input str,
        extensions: Extensions,
    ) -> Self {
        debug_assert!(
            line.is_empty()
                || (line.first().unwrap().span.start() < input.len()
                    && line.last().unwrap().span.end() <= input.len()),
            "tokens out of input bounds"
        );
        debug_assert!(
            line.windows(2)
                .all(|w| w[0].span.end() == w[1].span.start()),
            "tokens are not adjacent"
        );
        Self {
            base_offset,
            tokens: line,
            current: 0,
            input,
            context: Context::default(),
            extensions,
        }
    }

    /// Finish parsing the line, this will return the error/warning
    /// context used in the line.
    ///
    /// Panics if is inside a [Self::with_recover] or if any token is left.
    pub(crate) fn finish(self) -> Context<ParserError, ParserWarning> {
        assert_eq!(
            self.current,
            self.tokens.len(),
            "Line tokens not parsed. this is a bug"
        );
        self.context
    }

    pub(crate) fn extension(&self, ext: Extensions) -> bool {
        self.extensions.contains(ext)
    }

    /// Runs a function that can fail to parse the input.
    ///
    /// If the function succeeds, is just as it was called withtout recover.
    /// If the function fails, any token eaten by it will be restored.
    ///
    /// Note that any other state modification such as adding errors to the
    /// context will not be rolled back.
    pub(crate) fn with_recover<F, O>(&mut self, f: F) -> Option<O>
    where
        F: FnOnce(&mut Self) -> Option<O>,
    {
        let old_current = self.current;
        let r = f(self);
        if r.is_none() {
            self.current = old_current;
        }
        r
    }

    /// Gets a token's matching str from the input
    pub(crate) fn as_str(&self, token: Token) -> &'input str {
        &self.input[token.span.range()]
    }

    pub(crate) fn text(&self, offset: usize, tokens: &[Token]) -> ast::Text<'input> {
        debug_assert!(
            tokens
                .windows(2)
                .all(|w| w[0].span.end() == w[1].span.start()),
            "tokens are not adjacent"
        );

        let mut t = ast::Text::empty(offset);
        if tokens.is_empty() {
            return t;
        }
        let mut start = tokens[0].span.start();
        let mut end = start;
        assert_eq!(offset, start);

        for token in tokens {
            match token.kind {
                T![line comment] | T![block comment] => {
                    t.append_str(&self.input[start..end], start);
                    start = token.span.end();
                    end = start;
                }
                T![escaped] => {
                    t.append_str(&self.input[start..end], start);
                    debug_assert_eq!(token.len(), 2, "unexpected escaped token length");
                    start = token.span.start() + 1; // skip "\"
                    end = token.span.end()
                }
                _ => end = token.span.end(),
            }
        }
        t.append_str(&self.input[start..end], start);
        t
    }

    /// Returns the current offset from the start of input
    pub(crate) fn current_offset(&self) -> usize {
        self.parsed()
            .last()
            .map(|t| t.span.end())
            .unwrap_or(self.base_offset)
    }

    pub(crate) fn tokens_consumed(&self) -> usize {
        self.current
    }

    pub(crate) fn tokens(&self) -> &'t [Token] {
        self.tokens
    }

    pub(crate) fn parsed(&self) -> &'t [Token] {
        self.tokens.split_at(self.current).0
    }

    /// Returns the not parsed tokens
    pub(crate) fn rest(&self) -> &'t [Token] {
        self.tokens.split_at(self.current).1
    }

    pub(crate) fn consume_rest(&mut self) -> &'t [Token] {
        let r = self.rest();
        self.current += r.len();
        r
    }

    /// Peeks the next token without consuming it.
    pub(crate) fn peek(&self) -> TokenKind {
        self.tokens
            .get(self.current)
            .map(|token| token.kind)
            .unwrap_or(TokenKind::Eof)
    }

    /// Checks the next token without consuming it.
    pub(crate) fn at(&self, kind: TokenKind) -> bool {
        self.peek() == kind
    }

    /// Advance to the next token.
    #[must_use]
    pub(crate) fn next_token(&mut self) -> Option<Token> {
        if let Some(token) = self.tokens.get(self.current) {
            self.current += 1;
            Some(*token)
        } else {
            None
        }
    }

    /// Same as [Self::next_token] but panics if there are no more tokens.
    pub(crate) fn bump_any(&mut self) -> Token {
        self.next_token()
            .expect("Expected token, but there was none")
    }

    /// Call [Self::next_token] but panics if the next token is not `expected`.
    pub(crate) fn bump(&mut self, expected: TokenKind) -> Token {
        let token = self.bump_any();
        assert_eq!(
            token.kind, expected,
            "Expected '{expected:?}', but got '{:?}'",
            token.kind
        );
        token
    }

    /// Takes until condition reached, if never reached, return none
    pub(crate) fn until(&mut self, f: impl Fn(TokenKind) -> bool) -> Option<&'t [Token]> {
        let rest = self.rest();
        let pos = rest.iter().position(|t| f(t.kind))?;
        let s = &rest[..pos];
        self.current += pos;
        Some(s)
    }

    /// Consumes while the closure returns true or the line ends
    pub(crate) fn consume_while(&mut self, f: impl Fn(TokenKind) -> bool) -> &'t [Token] {
        let rest = self.rest();
        let pos = rest.iter().position(|t| !f(t.kind)).unwrap_or(rest.len());
        let s = &rest[..pos];
        self.current += pos;
        s
    }

    pub(crate) fn ws_comments(&mut self) -> &'t [Token] {
        self.consume_while(|t| matches!(t, T![ws] | T![line comment] | T![block comment]))
    }

    /// Call [Self::next_token] if the next token is `expected`.
    #[must_use]
    pub(crate) fn consume(&mut self, expected: TokenKind) -> Option<Token> {
        if self.at(expected) {
            Some(self.bump_any())
        } else {
            None
        }
    }

    pub(crate) fn error(&mut self, error: ParserError) {
        self.context.error(error);
    }
    pub(crate) fn warn(&mut self, warn: ParserWarning) {
        self.context.warn(warn)
    }
}

/// get the span for a slice of tokens. panics if the slice is empty
pub(crate) fn tokens_span(tokens: &[Token]) -> Span {
    debug_assert!(!tokens.is_empty(), "tokens_span tokens empty");
    let start = tokens.first().unwrap().span.start();
    let end = tokens.last().unwrap().span.end();
    Span::new(start, end)
}

// match token type
macro_rules! mt {
    ($($reprs:tt)|*) => {
        $(Token {
            kind: T![$reprs],
            ..
        })|+
    }
}
pub(crate) use mt;

/// Errors generated by [`parse`] and [`parse_metadata`].
#[derive(Debug, Error)]
pub enum ParserError {
    #[error("A {container} is missing: {what}")]
    ComponentPartMissing {
        container: &'static str,
        what: &'static str,
        expected_pos: Span,
    },

    #[error("A {container} cannot have: {what}")]
    ComponentPartNotAllowed {
        container: &'static str,
        what: &'static str,
        to_remove: Span,
        help: Option<&'static str>,
    },

    #[error("Invalid {container} {what}: {reason}")]
    ComponentPartInvalid {
        container: &'static str,
        what: &'static str,
        reason: &'static str,
        labels: Vec<(Span, Option<Cow<'static, str>>)>,
        help: Option<&'static str>,
    },

    #[error("Duplicate ingredient modifier: {dup}")]
    DuplicateModifiers { modifiers_span: Span, dup: String },

    #[error("Error parsing integer number")]
    ParseInt {
        bad_bit: Span,
        source: std::num::ParseIntError,
    },

    #[error("Error parsing decimal number")]
    ParseFloat {
        bad_bit: Span,
        source: std::num::ParseFloatError,
    },

    #[error("Division by zero")]
    DivisionByZero { bad_bit: Span },

    #[error("Quantity scaling conflict")]
    QuantityScalingConflict { bad_bit: Span },
}

/// Warnings generated by [`parse`] and [`parse_metadata`].
#[derive(Debug, Error)]
pub enum ParserWarning {
    #[error("Empty metadata value for key: {key}")]
    EmptyMetadataValue { key: Located<String> },
    #[error("A {container} cannot have {what}, it will be ignored")]
    ComponentPartIgnored {
        container: &'static str,
        what: &'static str,
        ignored: Span,
        help: Option<&'static str>,
    },
}

impl RichError for ParserError {
    fn labels(&self) -> Vec<(Span, Option<Cow<'static, str>>)> {
        use crate::error::label;
        match self {
            ParserError::ComponentPartMissing {
                expected_pos: component_span,
                what,
                ..
            } => {
                vec![label!(component_span, format!("expected {what}"))]
            }
            ParserError::ComponentPartNotAllowed { to_remove, .. } => {
                vec![label!(to_remove, "remove this")]
            }
            ParserError::ComponentPartInvalid { labels, .. } => labels.clone(),
            ParserError::DuplicateModifiers { modifiers_span, .. } => vec![label!(modifiers_span)],
            ParserError::ParseInt { bad_bit, .. } => vec![label!(bad_bit)],
            ParserError::ParseFloat { bad_bit, .. } => vec![label!(bad_bit)],
            ParserError::DivisionByZero { bad_bit } => vec![label!(bad_bit)],
            ParserError::QuantityScalingConflict { bad_bit } => vec![label!(bad_bit)],
        }
    }

    fn help(&self) -> Option<Cow<'static, str>> {
        use crate::error::help;
        match self {
            ParserError::ComponentPartNotAllowed { help, .. } => help!(opt help),
            ParserError::ComponentPartInvalid { help, .. } => help!(opt help),
            ParserError::DuplicateModifiers { .. } => help!("Remove duplicate modifiers"),
            ParserError::DivisionByZero { .. } => {
                help!("Change this please, we don't want an infinite amount of anything")
            }
            ParserError::QuantityScalingConflict { .. } => help!("A quantity cannot have the auto scaling marker (*) and have fixed values at the same time"),
            _ => None,
        }
    }

    fn code(&self) -> Option<&'static str> {
        Some("parser")
    }
}

impl RichError for ParserWarning {
    fn labels(&self) -> Vec<(Span, Option<Cow<'static, str>>)> {
        use crate::error::label;
        match self {
            ParserWarning::EmptyMetadataValue { key } => {
                vec![label!(key)]
            }
            ParserWarning::ComponentPartIgnored { ignored, .. } => {
                vec![label!(ignored, "this is ignored")]
            }
        }
    }

    fn help(&self) -> Option<Cow<'static, str>> {
        use crate::error::help;
        match self {
            ParserWarning::EmptyMetadataValue { .. } => None,
            ParserWarning::ComponentPartIgnored { help, .. } => help!(opt help),
        }
    }

    fn code(&self) -> Option<&'static str> {
        Some("parser")
    }

    fn kind(&self) -> ariadne::ReportKind {
        ariadne::ReportKind::Warning
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::*;

    #[test]
    fn just_metadata() {
        let (ast, warn, err) = parse_metadata(
            r#">> entry: true
a test @step @salt{1%mg} more text
a test @step @salt{1%mg} more text
a test @step @salt{1%mg} more text
>> entry2: uwu
a test @step @salt{1%mg} more text
"#,
        )
        .into_tuple();
        assert!(warn.is_empty());
        assert!(err.is_empty());
        assert_eq!(
            ast.unwrap().lines,
            vec![
                Line::Metadata {
                    key: Text::from_str(" entry", 2),
                    value: Text::from_str(" true", 10)
                },
                Line::Metadata {
                    key: Text::from_str(" entry2", 126),
                    value: Text::from_str(" uwu", 134)
                },
            ]
        );
    }

    #[test]
    fn multiline_spaces() {
        let (ast, warn, err) = parse(
            r#"  This is a step           -- comment
  and this line continues  -- another comment"#,
            Extensions::MULTILINE_STEPS,
        )
        .into_tuple();

        // Only whitespace between line should be trimmed
        assert!(warn.is_empty());
        assert!(err.is_empty());
        assert_eq!(
            ast.unwrap().lines,
            vec![Line::Step {
                is_text: false,
                items: vec![
                    Item::Text(Text::from_str("  This is a step", 0)),
                    Item::Text(Text::from_str(" ", 37)), // at the original end of the line
                    Item::Text(Text::from_str("and this line continues  ", 41))
                ]
            }]
        );
    }
}
