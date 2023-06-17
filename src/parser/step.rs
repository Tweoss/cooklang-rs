use crate::{
    ast::{self, Modifiers, Text},
    context::Recover,
    error::label,
    lexer::T,
    located::Located,
    span::Span,
    Extensions,
};

use super::{
    quantity::parse_quantity, token_stream::Token, tokens_span, LineParser, ParserError,
    ParserWarning,
};

pub struct ParsedStep<'input> {
    pub is_text: bool,
    pub items: Vec<ast::Item<'input>>,
}

pub(crate) fn step<'input>(
    line: &mut LineParser<'_, 'input>,
    force_text: bool,
) -> ParsedStep<'input> {
    let is_text = line.consume(T![>]).is_some();

    let mut items: Vec<ast::Item> = vec![];

    if is_text || force_text {
        let start = line.current_offset();
        let tokens = line.consume_rest();
        items.push(ast::Item::Text(line.text(start, tokens)));
        return ParsedStep { is_text, items };
    }

    while !line.rest().is_empty() {
        let start = line.current_offset();
        let component = match line.peek() {
            T![@] => line
                .with_recover(ingredient)
                .map(ast::Component::Ingredient),
            T![#] => line.with_recover(cookware).map(ast::Component::Cookware),
            T![~] => line.with_recover(timer).map(ast::Component::Timer),
            _ => None,
        };
        if let Some(component) = component {
            let end = line.current_offset();
            items.push(ast::Item::Component(Box::new(Located::new(
                component,
                Span::new(start, end),
            ))));
        } else {
            let tokens_start = line.tokens_consumed();
            line.bump_any(); // consume the first token, this avoids entering an infinite loop
            line.consume_while(|t| !matches!(t, T![@] | T![#] | T![~]));
            let tokens_end = line.tokens_consumed();
            let tokens = &line.tokens()[tokens_start..tokens_end];

            items.push(ast::Item::Text(line.text(start, tokens)));
        }
    }

    ParsedStep {
        is_text: false,
        items,
    }
}

struct Body<'t> {
    name: &'t [Token],
    close: Option<Span>,
    quantity: Option<&'t [Token]>,
}

fn comp_body<'t>(line: &mut LineParser<'t, '_>) -> Option<Body<'t>> {
    line.with_recover(|line| {
        let name = line.until(|t| matches!(t, T!['{'] | T![@] | T![#] | T![~]))?;
        let close_span_start = line.consume(T!['{'])?.span.start();
        let quantity = line.until(|t| t == T!['}'])?;
        let close_span_end = line.bump(T!['}']).span.end();
        let close_span = Span::new(close_span_start, close_span_end);
        if quantity
            .iter()
            .any(|t| !matches!(t.kind, T![ws] | T![block comment]))
        {
            Some(Body {
                name,
                close: Some(close_span),
                quantity: Some(quantity),
            })
        } else {
            Some(Body {
                name,
                close: Some(close_span),
                quantity: None,
            })
        }
    })
    .or_else(|| {
        line.with_recover(|line| {
            let tokens = line.consume_while(|t| matches!(t, T![word] | T![int] | T![float]));
            if tokens.is_empty() {
                return None;
            }
            Some(Body {
                name: tokens,
                close: None,
                quantity: None,
            })
        })
    })
}

fn modifiers<'t>(line: &mut LineParser<'t, '_>) -> &'t [Token] {
    line.consume_while(|t| matches!(t, T![@] | T![&] | T![?] | T![+] | T![-]))
}

fn note<'input>(line: &mut LineParser<'_, 'input>) -> Option<Text<'input>> {
    line.extension(Extensions::COMPONENT_NOTE)
        .then(|| {
            line.with_recover(|line| {
                line.consume(T!['('])?;
                let offset = line.current_offset();
                let note = line.until(|t| t == T![')'])?;
                line.bump(T![')']);
                Some(line.text(offset, note))
            })
        })
        .flatten()
}

fn parse_modifiers(
    line: &mut LineParser,
    modifiers_tokens: &[Token],
    modifiers_pos: usize,
) -> Located<Modifiers> {
    if modifiers_tokens.is_empty() {
        Located::new(Modifiers::empty(), Span::pos(modifiers_pos))
    } else if !line.extension(Extensions::COMPONENT_MODIFIERS) {
        let modifiers_span = tokens_span(modifiers_tokens);
        line.error(ParserError::ExtensionNotEnabled {
            span: modifiers_span,
            extension_name: "component modifiers",
        });
        Located::new(Modifiers::empty(), modifiers_span)
    } else {
        let modifiers_span = tokens_span(modifiers_tokens);
        let m = modifiers_tokens
            .iter()
            .try_fold(Modifiers::empty(), |acc, m| {
                let new_m = match m.kind {
                    T![@] => Modifiers::RECIPE,
                    T![&] => Modifiers::REF,
                    T![?] => Modifiers::OPT,
                    T![+] => Modifiers::NEW,
                    T![-] => Modifiers::HIDDEN,
                    _ => unreachable!(), // checked in [modifiers] function
                };

                if acc.contains(new_m) {
                    line.error(ParserError::DuplicateModifiers {
                        modifiers_span,
                        dup: line.as_str(*m).to_string(),
                    });
                    Err(())
                } else {
                    Ok(acc | new_m)
                }
            })
            .unwrap_or(Modifiers::empty());

        Located::new(m, modifiers_span)
    }
}

fn parse_alias<'input>(
    container: &'static str,
    line: &mut LineParser<'_, 'input>,
    tokens: &[Token],
    name_offset: usize,
) -> (Text<'input>, Option<Text<'input>>) {
    if let Some(alias_sep) = line
        .extension(Extensions::COMPONENT_ALIAS)
        .then(|| tokens.iter().position(|t| t.kind == T![|]))
        .flatten()
    {
        let (name_tokens, alias_tokens) = tokens.split_at(alias_sep);
        let (alias_sep, alias_text_tokens) = alias_tokens.split_first().unwrap();
        let alias_text = line.text(alias_sep.span.end(), alias_text_tokens);
        let alias_text = if alias_text_tokens.iter().any(|t| t.kind == T![|]) {
            let bad_bit = Span::new(
                alias_sep.span.start(),
                alias_text_tokens.last().unwrap_or(alias_sep).span.end(),
            );
            line.error(ParserError::ComponentPartInvalid {
                container,
                what: "alias",
                reason: "multiple aliases",
                labels: vec![label!(bad_bit, "more than one alias defined here")],
                help: Some("A component can only have one alias. Remove the extra '|'."),
            });
            None
        } else if alias_text.is_text_empty() {
            line.error(ParserError::ComponentPartInvalid {
                container,
                what: "alias",
                reason: "is empty",
                labels: vec![
                    label!(alias_sep.span, "remove this"),
                    label!(alias_text.span(), "or add something here"),
                ],
                help: None,
            });
            None
        } else {
            Some(alias_text)
        };
        (line.text(name_offset, name_tokens), alias_text)
    } else {
        (line.text(name_offset, tokens), None)
    }
}

const INGREDIENT: &str = "ingredient";
const COOKWARE: &str = "cookware";
const TIMER: &str = "timer";

fn ingredient<'input>(line: &mut LineParser<'_, 'input>) -> Option<ast::Ingredient<'input>> {
    // Parse
    line.consume(T![@])?;
    let modifiers_pos = line.current_offset();
    let modifiers_tokens = modifiers(line);
    let name_offset = line.current_offset();
    let body = comp_body(line)?;
    let note = note(line);

    // Build text(s) and checks
    let (name, alias) = parse_alias(INGREDIENT, line, body.name, name_offset);

    if name.is_text_empty() {
        line.error(ParserError::ComponentPartInvalid {
            container: INGREDIENT,
            what: "name",
            reason: "is empty",
            labels: vec![label!(name.span(), "add a name here")],
            help: None,
        });
    }

    let modifiers = parse_modifiers(line, modifiers_tokens, modifiers_pos);

    let quantity = body.quantity.map(|tokens| {
        parse_quantity(tokens, line.input, line.extensions, &mut line.context).quantity
    });

    Some(ast::Ingredient {
        modifiers,
        name,
        alias,
        quantity,
        note,
    })
}

fn cookware<'input>(line: &mut LineParser<'_, 'input>) -> Option<ast::Cookware<'input>> {
    // Parse
    line.consume(T![#])?;
    let modifiers_pos = line.current_offset();
    let modifiers_tokens = modifiers(line);
    let name_offset = line.current_offset();
    let body = comp_body(line)?;
    let note = note(line);

    // Errors
    let (name, alias) = parse_alias(COOKWARE, line, body.name, name_offset);
    if name.is_text_empty() {
        line.error(ParserError::ComponentPartInvalid {
            container: COOKWARE,
            what: "name",
            reason: "is empty",
            labels: vec![label!(name, "add a name here")],
            help: None,
        });
    }
    let quantity = body.quantity.map(|tokens| {
        let q = parse_quantity(tokens, line.input, line.extensions, &mut line.context);
        if let Some(unit) = &q.quantity.unit {
            let span = if let Some(sep) = q.unit_separator {
                Span::new(sep.start(), unit.span().end())
            } else {
                unit.span()
            };
            line.error(ParserError::ComponentPartNotAllowed {
                container: COOKWARE,
                what: "unit in quantity",
                to_remove: span,
                help: Some("Cookware quantity can't have an unit."),
            });
        }
        if let ast::QuantityValue::Single {
            auto_scale: Some(auto_scale),
            ..
        } = &q.quantity.value
        {
            line.error(ParserError::ComponentPartNotAllowed {
                container: COOKWARE,
                what: "auto scale marker",
                to_remove: *auto_scale,
                help: Some("Cookware quantity can't be auto scaled."),
            });
        }
        q.quantity.map_inner(|q| q.value)
    });
    let modifiers = parse_modifiers(line, modifiers_tokens, modifiers_pos);

    if modifiers.contains(Modifiers::RECIPE) {
        let pos = modifiers_tokens
            .iter()
            .find(|t| t.kind == T![@])
            .map(|t| t.span)
            .expect("no recipe token in modifiers with recipe");

        line.error(ParserError::ComponentPartInvalid {
            container: COOKWARE,
            what: "modifiers",
            reason: "recipe modifier not allowed in cookware",
            labels: vec![(pos, Some("remove this".into()))],
            help: None,
        });
    }

    Some(ast::Cookware {
        name,
        alias,
        quantity,
        modifiers,
        note,
    })
}

fn timer<'input>(line: &mut LineParser<'_, 'input>) -> Option<ast::Timer<'input>> {
    // Parse
    line.consume(T![~])?;
    let modifiers_tokens = modifiers(line);
    let name_offset = line.current_offset();
    let body = comp_body(line)?;

    // Errors
    check_modifiers(line, modifiers_tokens, TIMER);
    check_alias(line, body.name, TIMER);
    check_note(line, TIMER);

    let name = line.text(name_offset, body.name);

    let mut quantity = body.quantity.map(|tokens| {
        let q = parse_quantity(tokens, line.input, line.extensions, &mut line.context);
        if let ast::QuantityValue::Single {
            auto_scale: Some(auto_scale),
            ..
        } = &q.quantity.value
        {
            line.error(ParserError::ComponentPartNotAllowed {
                container: TIMER,
                what: "auto scale marker",
                to_remove: *auto_scale,
                help: Some("Timer quantity can't be auto scaled."),
            });
        }
        if q.quantity.unit.is_none() {
            line.error(ParserError::ComponentPartMissing {
                container: TIMER,
                what: "quantity unit",
                expected_pos: Span::pos(q.quantity.value.span().end()),
            });
        }
        q.quantity
    });

    if quantity.is_none() && line.extension(Extensions::TIMER_REQUIRES_TIME) {
        let span = body.close.unwrap_or_else(|| Span::pos(name.span().end()));
        line.error(ParserError::ComponentPartMissing {
            container: TIMER,
            what: "quantity",
            expected_pos: span,
        });
        quantity = Some(Recover::recover());
    }

    let name = if name.is_text_empty() {
        None
    } else {
        Some(name)
    };

    if name.is_none() && quantity.is_none() {
        let span = if let Some(s) = body.close {
            Span::new(name_offset, s.end())
        } else {
            Span::pos(name_offset)
        };
        line.error(ParserError::ComponentPartMissing {
            container: TIMER,
            what: "quantity OR name",
            expected_pos: span,
        });
        quantity = Some(Recover::recover()); // could be also name, but whatever
    }

    Some(ast::Timer { name, quantity })
}

fn check_modifiers(line: &mut LineParser, modifiers_tokens: &[Token], container: &'static str) {
    assert_ne!(container, INGREDIENT);
    if !modifiers_tokens.is_empty() {
        line.error(ParserError::ComponentPartNotAllowed {
            container,
            what: "modifiers",
            to_remove: tokens_span(modifiers_tokens),
            help: Some("Modifiers are only available in ingredients"),
        });
    }
}

fn check_alias(line: &mut LineParser, name_tokens: &[Token], container: &'static str) {
    assert_ne!(container, INGREDIENT);
    if let Some(sep) = name_tokens.iter().position(|t| t.kind == T![|]) {
        let to_remove = Span::new(
            name_tokens[sep].span.start(),
            name_tokens.last().unwrap().span.end(),
        );
        line.error(ParserError::ComponentPartNotAllowed {
            container,
            what: "alias",
            to_remove,
            help: Some("Aliases are only available in ingredients"),
        });
    }
}

fn check_note(line: &mut LineParser, container: &'static str) {
    assert_ne!(container, INGREDIENT);
    if !line.extension(Extensions::COMPONENT_NOTE) {
        return;
    }

    assert!(line
        .with_recover(|line| {
            let start = line.consume(T!['('])?.span.start();
            let _ = line.until(|t| t == T![')'])?;
            let end = line.bump(T![')']).span.end();
            line.warn(ParserWarning::ComponentPartIgnored {
                container,
                what: "note",
                ignored: Span::new(start, end),
                help: Some("Notes are only available in ingredients"),
            });
            None::<()> // always backtrack
        })
        .is_none());
}
