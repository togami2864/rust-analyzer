//! This module implements syntax validation that the parser doesn't handle.
//!
//! A failed validation emits a diagnostic.

mod block;

use rowan::Direction;
use rustc_lexer::unescape::{self, unescape_byte, unescape_char, unescape_literal, Mode};

use crate::{
    algo,
    ast::{self, HasAttrs, HasVisibility},
    match_ast, AstNode, SyntaxError,
    SyntaxKind::{CONST, FN, INT_NUMBER, TYPE_ALIAS},
    SyntaxNode, SyntaxToken, TextSize, T,
};

pub(crate) fn validate(root: &SyntaxNode) -> Vec<SyntaxError> {
    // FIXME:
    // * Add unescape validation of raw string literals and raw byte string literals
    // * Add validation of doc comments are being attached to nodes

    let mut errors = Vec::new();
    for node in root.descendants() {
        match_ast! {
            match node {
                ast::Literal(it) => validate_literal(it, &mut errors),
                ast::Const(it) => validate_const(it, &mut errors),
                ast::BlockExpr(it) => block::validate_block_expr(it, &mut errors),
                ast::FieldExpr(it) => validate_numeric_name(it.name_ref(), &mut errors),
                ast::RecordExprField(it) => validate_numeric_name(it.name_ref(), &mut errors),
                ast::Visibility(it) => validate_visibility(it, &mut errors),
                ast::RangeExpr(it) => validate_range_expr(it, &mut errors),
                ast::PathSegment(it) => validate_path_keywords(it, &mut errors),
                ast::RefType(it) => validate_trait_object_ref_ty(it, &mut errors),
                ast::PtrType(it) => validate_trait_object_ptr_ty(it, &mut errors),
                ast::FnPtrType(it) => validate_trait_object_fn_ptr_ret_ty(it, &mut errors),
                ast::MacroRules(it) => validate_macro_rules(it, &mut errors),
                ast::LetExpr(it) => validate_let_expr(it, &mut errors),
                _ => (),
            }
        }
    }
    errors
}

fn rustc_unescape_error_to_string(err: unescape::EscapeError) -> &'static str {
    use unescape::EscapeError as EE;

    #[rustfmt::skip]
    let err_message = match err {
        EE::ZeroChars => {
            "Literal must not be empty"
        }
        EE::MoreThanOneChar => {
            "Literal must be one character long"
        }
        EE::LoneSlash => {
            "Character must be escaped: `\\`"
        }
        EE::InvalidEscape => {
            "Invalid escape"
        }
        EE::BareCarriageReturn | EE::BareCarriageReturnInRawString => {
            "Character must be escaped: `\r`"
        }
        EE::EscapeOnlyChar => {
            "Escape character `\\` must be escaped itself"
        }
        EE::TooShortHexEscape => {
            "ASCII hex escape code must have exactly two digits"
        }
        EE::InvalidCharInHexEscape => {
            "ASCII hex escape code must contain only hex characters"
        }
        EE::OutOfRangeHexEscape => {
            "ASCII hex escape code must be at most 0x7F"
        }
        EE::NoBraceInUnicodeEscape => {
            "Missing `{` to begin the unicode escape"
        }
        EE::InvalidCharInUnicodeEscape => {
            "Unicode escape must contain only hex characters and underscores"
        }
        EE::EmptyUnicodeEscape => {
            "Unicode escape must not be empty"
        }
        EE::UnclosedUnicodeEscape => {
            "Missing `}` to terminate the unicode escape"
        }
        EE::LeadingUnderscoreUnicodeEscape => {
            "Unicode escape code must not begin with an underscore"
        }
        EE::OverlongUnicodeEscape => {
            "Unicode escape code must have at most 6 digits"
        }
        EE::LoneSurrogateUnicodeEscape => {
            "Unicode escape code must not be a surrogate"
        }
        EE::OutOfRangeUnicodeEscape => {
            "Unicode escape code must be at most 0x10FFFF"
        }
        EE::UnicodeEscapeInByte => {
            "Byte literals must not contain unicode escapes"
        }
        EE::NonAsciiCharInByte | EE::NonAsciiCharInByteString => {
            "Byte literals must not contain non-ASCII characters"
        }
    };

    err_message
}

fn validate_literal(literal: ast::Literal, acc: &mut Vec<SyntaxError>) {
    // FIXME: move this function to outer scope (https://github.com/rust-lang/rust-analyzer/pull/2834#discussion_r366196658)
    fn unquote(text: &str, prefix_len: usize, end_delimiter: char) -> Option<&str> {
        text.rfind(end_delimiter).and_then(|end| text.get(prefix_len..end))
    }

    let token = literal.token();
    let text = token.text();

    // FIXME: lift this lambda refactor to `fn` (https://github.com/rust-lang/rust-analyzer/pull/2834#discussion_r366199205)
    let mut push_err = |prefix_len, (off, err): (usize, unescape::EscapeError)| {
        let off = token.text_range().start() + TextSize::try_from(off + prefix_len).unwrap();
        acc.push(SyntaxError::new_at_offset(rustc_unescape_error_to_string(err), off));
    };

    match literal.kind() {
        ast::LiteralKind::String(s) => {
            if !s.is_raw() {
                if let Some(without_quotes) = unquote(text, 1, '"') {
                    unescape_literal(without_quotes, Mode::Str, &mut |range, char| {
                        if let Err(err) = char {
                            push_err(1, (range.start, err));
                        }
                    });
                }
            }
        }
        ast::LiteralKind::ByteString(s) => {
            if !s.is_raw() {
                if let Some(without_quotes) = unquote(text, 2, '"') {
                    unescape_literal(without_quotes, Mode::ByteStr, &mut |range, char| {
                        if let Err(err) = char {
                            push_err(2, (range.start, err));
                        }
                    });
                }
            }
        }
        ast::LiteralKind::Char(_) => {
            if let Some(Err(e)) = unquote(text, 1, '\'').map(unescape_char) {
                push_err(1, e);
            }
        }
        ast::LiteralKind::Byte(_) => {
            if let Some(Err(e)) = unquote(text, 2, '\'').map(unescape_byte) {
                push_err(2, e);
            }
        }
        ast::LiteralKind::IntNumber(_)
        | ast::LiteralKind::FloatNumber(_)
        | ast::LiteralKind::Bool(_) => {}
    }
}

pub(crate) fn validate_block_structure(root: &SyntaxNode) {
    let mut stack = Vec::new();
    for node in root.descendants_with_tokens() {
        match node.kind() {
            T!['{'] => stack.push(node),
            T!['}'] => {
                if let Some(pair) = stack.pop() {
                    assert_eq!(
                        node.parent(),
                        pair.parent(),
                        "\nunpaired curlies:\n{}\n{:#?}\n",
                        root.text(),
                        root,
                    );
                    assert!(
                        node.next_sibling_or_token().is_none()
                            && pair.prev_sibling_or_token().is_none(),
                        "\nfloating curlies at {:?}\nfile:\n{}\nerror:\n{}\n",
                        node,
                        root.text(),
                        node,
                    );
                }
            }
            _ => (),
        }
    }
}

fn validate_numeric_name(name_ref: Option<ast::NameRef>, errors: &mut Vec<SyntaxError>) {
    if let Some(int_token) = int_token(name_ref) {
        if int_token.text().chars().any(|c| !c.is_ascii_digit()) {
            errors.push(SyntaxError::new(
                "Tuple (struct) field access is only allowed through \
                decimal integers with no underscores or suffix",
                int_token.text_range(),
            ));
        }
    }

    fn int_token(name_ref: Option<ast::NameRef>) -> Option<SyntaxToken> {
        name_ref?.syntax().first_child_or_token()?.into_token().filter(|it| it.kind() == INT_NUMBER)
    }
}

fn validate_visibility(vis: ast::Visibility, errors: &mut Vec<SyntaxError>) {
    let path_without_in_token = vis.in_token().is_none()
        && vis.path().and_then(|p| p.as_single_name_ref()).and_then(|n| n.ident_token()).is_some();
    if path_without_in_token {
        errors.push(SyntaxError::new("incorrect visibility restriction", vis.syntax.text_range()));
    }
    let parent = match vis.syntax().parent() {
        Some(it) => it,
        None => return,
    };
    match parent.kind() {
        FN | CONST | TYPE_ALIAS => (),
        _ => return,
    }

    let impl_def = match parent.parent().and_then(|it| it.parent()).and_then(ast::Impl::cast) {
        Some(it) => it,
        None => return,
    };
    // FIXME: disable validation if there's an attribute, since some proc macros use this syntax.
    // ideally the validation would run only on the fully expanded code, then this wouldn't be necessary.
    if impl_def.trait_().is_some() && impl_def.attrs().next().is_none() {
        errors.push(SyntaxError::new("Unnecessary visibility qualifier", vis.syntax.text_range()));
    }
}

fn validate_range_expr(expr: ast::RangeExpr, errors: &mut Vec<SyntaxError>) {
    if expr.op_kind() == Some(ast::RangeOp::Inclusive) && expr.end().is_none() {
        errors.push(SyntaxError::new(
            "An inclusive range must have an end expression",
            expr.syntax().text_range(),
        ));
    }
}

fn validate_path_keywords(segment: ast::PathSegment, errors: &mut Vec<SyntaxError>) {
    let path = segment.parent_path();
    let is_path_start = segment.coloncolon_token().is_none() && path.qualifier().is_none();

    if let Some(token) = segment.self_token() {
        if !is_path_start {
            errors.push(SyntaxError::new(
                "The `self` keyword is only allowed as the first segment of a path",
                token.text_range(),
            ));
        }
    } else if let Some(token) = segment.crate_token() {
        if !is_path_start || use_prefix(path).is_some() {
            errors.push(SyntaxError::new(
                "The `crate` keyword is only allowed as the first segment of a path",
                token.text_range(),
            ));
        }
    }

    fn use_prefix(mut path: ast::Path) -> Option<ast::Path> {
        for node in path.syntax().ancestors().skip(1) {
            match_ast! {
                match node {
                    ast::UseTree(it) => if let Some(tree_path) = it.path() {
                        // Even a top-level path exists within a `UseTree` so we must explicitly
                        // allow our path but disallow anything else
                        if tree_path != path {
                            return Some(tree_path);
                        }
                    },
                    ast::UseTreeList(_) => continue,
                    ast::Path(parent) => path = parent,
                    _ => return None,
                }
            };
        }
        None
    }
}

fn validate_trait_object_ref_ty(ty: ast::RefType, errors: &mut Vec<SyntaxError>) {
    if let Some(ast::Type::DynTraitType(ty)) = ty.ty() {
        if let Some(err) = validate_trait_object_ty(ty) {
            errors.push(err);
        }
    }
}

fn validate_trait_object_ptr_ty(ty: ast::PtrType, errors: &mut Vec<SyntaxError>) {
    if let Some(ast::Type::DynTraitType(ty)) = ty.ty() {
        if let Some(err) = validate_trait_object_ty(ty) {
            errors.push(err);
        }
    }
}

fn validate_trait_object_fn_ptr_ret_ty(ty: ast::FnPtrType, errors: &mut Vec<SyntaxError>) {
    if let Some(ast::Type::DynTraitType(ty)) = ty.ret_type().and_then(|ty| ty.ty()) {
        if let Some(err) = validate_trait_object_ty(ty) {
            errors.push(err);
        }
    }
}

fn validate_trait_object_ty(ty: ast::DynTraitType) -> Option<SyntaxError> {
    let tbl = ty.type_bound_list()?;

    if tbl.bounds().count() > 1 {
        let dyn_token = ty.dyn_token()?;
        let potential_parenthesis =
            algo::skip_trivia_token(dyn_token.prev_token()?, Direction::Prev)?;
        let kind = potential_parenthesis.kind();
        if !matches!(kind, T!['('] | T![<] | T![=]) {
            return Some(SyntaxError::new("ambiguous `+` in a type", ty.syntax().text_range()));
        }
    }
    None
}

fn validate_macro_rules(mac: ast::MacroRules, errors: &mut Vec<SyntaxError>) {
    if let Some(vis) = mac.visibility() {
        errors.push(SyntaxError::new(
            "visibilities are not allowed on `macro_rules!` items",
            vis.syntax().text_range(),
        ));
    }
}

fn validate_const(const_: ast::Const, errors: &mut Vec<SyntaxError>) {
    if let Some(mut_token) = const_
        .const_token()
        .and_then(|t| t.next_token())
        .and_then(|t| algo::skip_trivia_token(t, Direction::Next))
        .filter(|t| t.kind() == T![mut])
    {
        errors.push(SyntaxError::new("const globals cannot be mutable", mut_token.text_range()));
    }
}

fn validate_let_expr(let_: ast::LetExpr, errors: &mut Vec<SyntaxError>) {
    let mut token = let_.syntax().clone();
    loop {
        token = match token.parent() {
            Some(it) => it,
            None => break,
        };

        if ast::ParenExpr::can_cast(token.kind()) {
            continue;
        } else if let Some(it) = ast::BinExpr::cast(token.clone()) {
            if it.op_kind() == Some(ast::BinaryOp::LogicOp(ast::LogicOp::And)) {
                continue;
            }
        } else if ast::IfExpr::can_cast(token.kind())
            || ast::WhileExpr::can_cast(token.kind())
            || ast::MatchGuard::can_cast(token.kind())
        {
            // It must be part of the condition since the expressions are inside a block.
            return;
        }

        break;
    }
    errors.push(SyntaxError::new(
        "`let` expressions are not supported here",
        let_.syntax().text_range(),
    ));
}
