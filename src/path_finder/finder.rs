//! Finds paths in a Moodle file that refer to another file in the codebase and converts them to
//! a standard format.
//!
//! Two glyphs are used for the two roots that exist since Moodle 5.1:
//!   - '#' is the repository root ($CFG->root), the parent of $CFG->dirroot.
//!   - '@' is $CFG->dirroot, which from Moodle 5.1 is the 'public/' subdirectory of the repo.
//!
//! Pre-5.1 the two coincide (dirroot prefix is '') and only '@' appears.
//!
//! Everything is resolved against the real repository root first, producing a '#'-rooted path;
//! the final step converts the dirroot portion back to '@' (e.g. '#/public/lib/setup.php' becomes
//! '@/lib/setup.php'). Files that live above dirroot keep their '#' path.
//!
//! Detection: recognises paths starting with $CFG->dirroot, $CFG->libdir, $CFG->root, __DIR__,
//! __FILE__, dirname(__FILE__), or an interpolated string whose first interpolated value is
//! $CFG->dirroot/libdir/root. For concat chains, the leftmost leaf is checked so 'a . b . c' is
//! identified by its first element.
//!
//! Two further, much more speculative, kinds of literal are also recognised, both resolved
//! relative to the current file's directory (as if implicitly prefixed with `__DIR__ . '/'`):
//!   - A bare string literal used as the *sole* value of a require/include construct — no
//!     concatenation, no wrapping expression — is treated as definitely a path.
//!   - A string literal starting with '../', standalone or as the leading operand of a
//!     concatenation, is also recorded, but with no such certainty.
//!
//! Every result records a [`super::PathKind`] identifying which of these anchored it.
//!
//! Extraction: maps each node in the expression to a path segment, relative to the repository
//! root:
//!   - $CFG->dirroot => the dirroot prefix (e.g. '/public'), $CFG->libdir => '<dirroot>/lib',
//!     $CFG->root => '' (the bare repository root), $CFG->admin => 'admin' (the bare directory
//!     name, always used as "$CFG->dirroot/$CFG->admin")
//!   - __DIR__ => '/dir/of/current/file', dirname(__FILE__) => directory N levels up
//!   - String literals => their value
//!   - Interpolated strings => recursively built from their parts
//!   - DIRECTORY_SEPARATOR => '/', other PHP constants => {CONSTANT_NAME}
//!   - Any other expression => its exact source text, wrapped in {}. Unlike a re-serialising
//!     pretty-printer, mago's CST keeps explicit parentheses as real `Parenthesized` nodes, so a
//!     verbatim source slice already carries any grouping the original author wrote — no
//!     precedence-aware re-rendering is needed.
//!
//! Normalisation: resolves '.' and '..' segments, rejoins with a leading '/', prepends '#', then
//! converts the dirroot portion to '@'.

use std::borrow::Cow;

use mago_allocator::LocalArena;
use mago_database::file::File;
use mago_database::file::FileType;
use mago_span::HasSpan;
use mago_span::Span;
use mago_syntax::cst::Access;
use mago_syntax::cst::Assignment;
use mago_syntax::cst::Binary;
use mago_syntax::cst::BinaryOperator;
use mago_syntax::cst::Call;
use mago_syntax::cst::ClassLikeMemberSelector;
use mago_syntax::cst::CompositeString;
use mago_syntax::cst::ConstantAccess;
use mago_syntax::cst::Expression;
use mago_syntax::cst::FunctionCall;
use mago_syntax::cst::Identifier;
use mago_syntax::cst::IncludeConstruct;
use mago_syntax::cst::IncludeOnceConstruct;
use mago_syntax::cst::InterpolatedString;
use mago_syntax::cst::Literal;
use mago_syntax::cst::MagicConstant;
use mago_syntax::cst::MethodCall;
use mago_syntax::cst::PropertyAccess;
use mago_syntax::cst::RequireConstruct;
use mago_syntax::cst::RequireOnceConstruct;
use mago_syntax::cst::StaticMethodCall;
use mago_syntax::cst::StringPart;
use mago_syntax::cst::Variable;
use mago_syntax::walker;
use mago_syntax::walker::Walker;

use super::PathKind;
use super::PathNotation;
use super::PathResult;

/// Parses `source` (a full PHP file, including the `<?php` tag) and returns every internal-path
/// reference found in it. `current_file` is the path of the file relative to the repository
/// root, used to resolve `__DIR__`/`__FILE__`/`dirname(...)`.
pub fn find_paths(source: &str, current_file: &str, notation: &PathNotation) -> Vec<PathResult> {
    let arena = LocalArena::new();
    let file = File::new(
        Cow::Owned(current_file.as_bytes().to_vec()),
        FileType::Host,
        None,
        Cow::Owned(source.as_bytes().to_vec()),
    );
    let program = mago_syntax::parser::parse_file(&arena, &file);

    let mut context = Context { file: &file, current_file, notation, pending_parent: None, paths: Vec::new() };
    let walker = PathFindingWalker;
    for statement in &program.statements {
        walker.walk_statement(statement, &mut context);
    }
    context.paths
}

struct Context<'a> {
    file: &'a File,
    current_file: &'a str,
    notation: &'a PathNotation,
    /// Set immediately before walking an expression that sits in a known "parent" position
    /// (an include/require value, or a call argument); consumed the moment that expression is
    /// visited, so it never leaks into nested recursion.
    pending_parent: Option<String>,
    paths: Vec<PathResult>,
}

struct PathFindingWalker;

impl<'ast, 'arena, 'ctx> Walker<'ast, 'arena, Context<'ctx>> for PathFindingWalker {
    fn walk_expression(&self, expression: &'ast Expression<'arena>, context: &mut Context<'ctx>) {
        // Parentheses carry no meaning of their own, so looking through them must not disturb a
        // pending parent (an include/require value, or a call argument): recursing into the
        // generic walker below would otherwise consume it here, on the wrapper itself, and reach
        // the expression it actually wraps with no parent at all — e.g.
        // `require_once($CFG->libdir . '/classes/component.php')` (parenthesised, as
        // require/include's call-like form always is) would lose its "require_once" tag.
        if let Expression::Parenthesized(parenthesized) = expression {
            return self.walk_expression(parenthesized.expression, context);
        }

        let parent = context.pending_parent.take();
        if is_potential_path(expression) {
            context.paths.push(build_result(expression, parent, context));
            return;
        }
        walker::walk_expression(self, expression, context);
    }

    fn walk_assignment(&self, assignment: &'ast Assignment<'arena>, context: &mut Context<'ctx>) {
        // The left-hand side of an assignment defines the variable rather than referencing a
        // file, so it must never be recorded as a path — but it may still contain nested matches
        // (e.g. inside a subscript), so it is walked, just without checking the top node itself.
        if is_potential_path(assignment.lhs) {
            walker::walk_expression(self, assignment.lhs, context);
        } else {
            self.walk_expression(assignment.lhs, context);
        }
        self.walk_expression(assignment.rhs, context);
    }

    fn walk_include_construct(&self, construct: &'ast IncludeConstruct<'arena>, context: &mut Context<'ctx>) {
        if let Some(result) = build_require_literal_result(construct.value, Some("include".to_string()), context) {
            context.paths.push(result);
            return;
        }
        context.pending_parent = Some("include".to_string());
        self.walk_expression(construct.value, context);
    }

    fn walk_include_once_construct(&self, construct: &'ast IncludeOnceConstruct<'arena>, context: &mut Context<'ctx>) {
        if let Some(result) = build_require_literal_result(construct.value, Some("include_once".to_string()), context) {
            context.paths.push(result);
            return;
        }
        context.pending_parent = Some("include_once".to_string());
        self.walk_expression(construct.value, context);
    }

    fn walk_require_construct(&self, construct: &'ast RequireConstruct<'arena>, context: &mut Context<'ctx>) {
        if let Some(result) = build_require_literal_result(construct.value, Some("require".to_string()), context) {
            context.paths.push(result);
            return;
        }
        context.pending_parent = Some("require".to_string());
        self.walk_expression(construct.value, context);
    }

    fn walk_require_once_construct(&self, construct: &'ast RequireOnceConstruct<'arena>, context: &mut Context<'ctx>) {
        if let Some(result) = build_require_literal_result(construct.value, Some("require_once".to_string()), context) {
            context.paths.push(result);
            return;
        }
        context.pending_parent = Some("require_once".to_string());
        self.walk_expression(construct.value, context);
    }

    fn walk_function_call(&self, call: &'ast FunctionCall<'arena>, context: &mut Context<'ctx>) {
        self.walk_expression(call.function, context);
        let name = function_call_name(call.function);
        for argument in &call.argument_list.arguments {
            context.pending_parent = name.clone();
            self.walk_expression(argument.value(), context);
        }
    }

    fn walk_method_call(&self, call: &'ast MethodCall<'arena>, context: &mut Context<'ctx>) {
        self.walk_expression(call.object, context);
        walker::walk_class_like_member_selector(self, &call.method, context);
        let name = member_selector_name(&call.method);
        for argument in &call.argument_list.arguments {
            context.pending_parent = name.clone();
            self.walk_expression(argument.value(), context);
        }
    }

    fn walk_static_method_call(&self, call: &'ast StaticMethodCall<'arena>, context: &mut Context<'ctx>) {
        self.walk_expression(call.class, context);
        walker::walk_class_like_member_selector(self, &call.method, context);
        let name = static_call_name(call.class, &call.method);
        for argument in &call.argument_list.arguments {
            context.pending_parent = name.clone();
            self.walk_expression(argument.value(), context);
        }
    }
}

fn build_result(expr: &Expression<'_>, parent: Option<String>, context: &Context<'_>) -> PathResult {
    // `is_potential_path` guarantees `extract_path` returns `Some`.
    let (real_path, path) = extract_path(expr, context).expect("potential path must be extractable");
    build_path_result(
        expr,
        parent,
        context,
        classify_kind(expr),
        real_path,
        path,
        extract_separator(expr),
        extract_mono_path_expr(expr, context),
    )
}

/// If `value` is a bare string literal — no concatenation, no other wrapping expression, modulo
/// redundant parentheses — it is treated as definitely a path: resolved relative to the current
/// file's directory, as if it were `__DIR__ . '/' . $value`.
fn build_require_literal_result(value: &Expression<'_>, parent: Option<String>, context: &Context<'_>) -> Option<PathResult> {
    let literal = unwrap_parens(value);
    if !matches!(literal, Expression::Literal(Literal::String(_))) {
        return None;
    }
    let repo_relative = PathNotation::normalise(&dirname_relative(&node_to_segment(literal, context), context));
    let glyph = context.notation.to_glyph(&repo_relative);
    Some(build_path_result(
        literal,
        parent,
        context,
        PathKind::RequireLiteral,
        repo_relative.trim_start_matches('/').to_string(),
        glyph,
        String::new(),
        String::new(),
    ))
}

fn build_path_result(
    expr: &Expression<'_>,
    parent: Option<String>,
    context: &Context<'_>,
    kind: PathKind,
    real_path: String,
    path: String,
    separator: String,
    mono_path_expr: String,
) -> PathResult {
    let span = expr.span();
    let source = context.file.contents.as_ref();
    PathResult {
        path,
        real_path,
        kind,
        line: context.file.line_number(span.start_offset()) + 1,
        code: source_text(source, span).into_owned(),
        parent,
        start_pos: Some(span.start_offset() as usize),
        end_pos: Some(span.end_offset() as usize),
        separator,
        mono_path_expr,
    }
}

/// Strips any redundant parentheses around `expr` (e.g. `require(('x.php'))` is just `'x.php'`).
/// Elsewhere in this module, walking a `Parenthesized` node's child achieves the same thing for
/// free by simply recursing into it; this is only needed where that recursion is bypassed.
fn unwrap_parens<'e>(mut expr: &'e Expression<'e>) -> &'e Expression<'e> {
    while let Expression::Parenthesized(parenthesized) = expr {
        expr = parenthesized.expression;
    }
    expr
}

/// If `expr` is a string-concatenation `Binary`, its `(lhs, rhs)`.
fn as_concat<'e>(expr: &'e Expression<'e>) -> Option<(&'e Expression<'e>, &'e Expression<'e>)> {
    match expr {
        Expression::Binary(Binary { operator: BinaryOperator::StringConcat(_), lhs, rhs, .. }) => Some((lhs, rhs)),
        _ => None,
    }
}

/// True if `node` is the root of an expression that resolves to a path within the codebase.
///
/// The interpolated-string check (both here and as a concat's leftmost leaf) only ever
/// recognises $CFG->dirroot/libdir/root, not the wider set below — an interpolated string can
/// only ever *start* with a literal or an expression, and only the latter can meaningfully be one
/// of these roots.
pub(super) fn is_potential_path<'e>(expr: &'e Expression<'e>) -> bool {
    if as_concat(expr).is_some() {
        let leftmost = leftmost_concat_leaf(expr);
        return is_path_root(leftmost)
            || matches!(
                leftmost,
                Expression::CompositeString(CompositeString::Interpolated(interpolated))
                    if first_meaningful_expr(interpolated).is_some_and(is_cfg_dirroot_libdir_or_root)
            );
    }
    match expr {
        Expression::CompositeString(CompositeString::Interpolated(interpolated)) => {
            first_meaningful_expr(interpolated).is_some_and(is_cfg_dirroot_libdir_or_root)
        }
        Expression::Access(Access::Property(_)) => is_cfg_dirroot_libdir_or_root(expr),
        _ => false,
    }
}

/// True if `expr` is one of the roots recognised as the leading operand of a concatenation:
/// $CFG->dirroot/libdir/root, or __DIR__/__FILE__ (or a dirname(...) chain over them).
fn is_path_root(expr: &Expression<'_>) -> bool {
    is_cfg_dirroot_libdir_or_root(expr)
        || matches!(expr, Expression::MagicConstant(MagicConstant::Directory(_)))
        || matches!(expr, Expression::MagicConstant(MagicConstant::File(_)))
        || is_dirname_of_file_or_dir(expr)
}

fn is_cfg_variable(expr: &Expression<'_>) -> bool {
    matches!(expr, Expression::Variable(Variable::Direct(variable)) if variable.name == b"$CFG")
}

fn is_cfg_property(expr: &Expression<'_>, name: &[u8]) -> bool {
    let Expression::Access(Access::Property(PropertyAccess { object, property, .. })) = expr else {
        return false;
    };
    is_cfg_variable(object) && matches!(property, ClassLikeMemberSelector::Identifier(id) if id.value == name)
}

fn is_cfg_dirroot_libdir_or_root(expr: &Expression<'_>) -> bool {
    is_cfg_dirroot(expr) || is_cfg_libdir(expr) || is_cfg_root(expr)
}

fn is_cfg_dirroot(expr: &Expression<'_>) -> bool {
    is_cfg_property(expr, b"dirroot")
}

fn is_cfg_libdir(expr: &Expression<'_>) -> bool {
    is_cfg_property(expr, b"libdir")
}

fn is_cfg_root(expr: &Expression<'_>) -> bool {
    is_cfg_property(expr, b"root")
}

/// Determines which root identified an already-confirmed potential path (see
/// [`is_potential_path`]/[`is_path_root`]).
fn classify_kind(expr: &Expression<'_>) -> PathKind {
    let mut anchor = if as_concat(expr).is_some() {
        leftmost_concat_leaf(expr)
    } else if let Expression::CompositeString(CompositeString::Interpolated(interpolated)) = expr {
        first_meaningful_expr(interpolated).expect("is_potential_path guarantees an anchor expression")
    } else {
        expr
    };
    // The leftmost leaf of a concat can itself be an interpolated string (e.g.
    // `"{$CFG->dirroot}" . $x`); unwrap one more level to reach the actual anchor.
    if let Expression::CompositeString(CompositeString::Interpolated(interpolated)) = anchor {
        anchor = first_meaningful_expr(interpolated).unwrap_or(anchor);
    }

    if is_cfg_dirroot(anchor) {
        PathKind::Dirroot
    } else if is_cfg_libdir(anchor) {
        PathKind::Libdir
    } else if is_cfg_root(anchor) {
        PathKind::Root
    } else if matches!(anchor, Expression::MagicConstant(MagicConstant::File(_))) {
        PathKind::File
    } else if matches!(anchor, Expression::MagicConstant(MagicConstant::Directory(_))) || is_dirname_of_file_or_dir(anchor) {
        PathKind::Dir
    } else {
        // is_potential_path/is_path_root guarantee the anchor is one of the cases above.
        unreachable!("unrecognised path root")
    }
}

/// True if `node` is the $CFG->dirroot root of a concat: either $CFG->dirroot itself, or the
/// interpolated string "{$CFG->dirroot}" used as a leading concat operand.
fn is_dirroot_root(expr: &Expression<'_>) -> bool {
    if is_cfg_dirroot(expr) {
        return true;
    }
    let Expression::CompositeString(CompositeString::Interpolated(interpolated)) = expr else {
        return false;
    };
    let parts = meaningful_parts(interpolated);
    parts.len() == 1 && string_part_expr(parts[0]).is_some_and(is_cfg_dirroot)
}

fn leftmost_concat_leaf<'e>(expr: &'e Expression<'e>) -> &'e Expression<'e> {
    match as_concat(expr) {
        Some((lhs, _)) => leftmost_concat_leaf(lhs),
        None => expr,
    }
}

fn flatten_concat<'e>(expr: &'e Expression<'e>, out: &mut Vec<&'e Expression<'e>>) {
    match as_concat(expr) {
        Some((lhs, rhs)) => {
            flatten_concat(lhs, out);
            flatten_concat(rhs, out);
        }
        None => out.push(expr),
    }
}

fn is_dirname_of_file_or_dir(expr: &Expression<'_>) -> bool {
    let Expression::Call(Call::Function(call)) = expr else {
        return false;
    };
    let Expression::Identifier(id) = call.function else {
        return false;
    };
    if identifier_name(id) != "dirname" {
        return false;
    }
    let mut args = call.argument_list.arguments.iter();
    let (Some(first), None) = (args.next(), args.next()) else {
        return false;
    };
    let arg = first.value();
    matches!(arg, Expression::MagicConstant(MagicConstant::File(_)))
        || matches!(arg, Expression::MagicConstant(MagicConstant::Directory(_)))
        || is_dirname_of_file_or_dir(arg)
}

/// String-part entries with no source text (mago inserts these as boundary markers around
/// interpolated expressions, e.g. between two adjacent `$a$b`); dropping them yields the same
/// part list PHP-Parser produces, which the positional checks below (first part, second part)
/// assume.
fn meaningful_parts<'e>(interpolated: &'e InterpolatedString<'e>) -> Vec<&'e StringPart<'e>> {
    interpolated.parts.iter().filter(|part| !matches!(part, StringPart::Literal(l) if l.raw.is_empty())).collect()
}

fn string_part_expr<'e>(part: &'e StringPart<'e>) -> Option<&'e Expression<'e>> {
    match part {
        StringPart::Expression(expr) => Some(expr),
        StringPart::BracedExpression(braced) => Some(braced.expression),
        StringPart::Literal(_) => None,
    }
}

fn first_meaningful_expr<'e>(interpolated: &'e InterpolatedString<'e>) -> Option<&'e Expression<'e>> {
    meaningful_parts(interpolated).first().copied().and_then(string_part_expr)
}

fn identifier_name(id: &Identifier<'_>) -> String {
    let bytes = id.value();
    let bytes = bytes.strip_prefix(b"\\").unwrap_or(bytes);
    String::from_utf8_lossy(bytes).into_owned()
}

fn member_selector_name(selector: &ClassLikeMemberSelector<'_>) -> Option<String> {
    match selector {
        ClassLikeMemberSelector::Identifier(id) => Some(String::from_utf8_lossy(id.value).into_owned()),
        _ => None,
    }
}

fn function_call_name(function: &Expression<'_>) -> Option<String> {
    match function {
        Expression::Identifier(id) => Some(identifier_name(id)),
        _ => None,
    }
}

fn static_call_class_name(class: &Expression<'_>) -> Option<String> {
    match class {
        Expression::Identifier(id) => Some(identifier_name(id)),
        Expression::Self_(keyword) | Expression::Static(keyword) | Expression::Parent(keyword) => {
            Some(String::from_utf8_lossy(keyword.value).into_owned())
        }
        _ => None,
    }
}

fn static_call_name(class: &Expression<'_>, method: &ClassLikeMemberSelector<'_>) -> Option<String> {
    let class_name = static_call_class_name(class);
    let method_name = member_selector_name(method);
    match (class_name, method_name) {
        (Some(class), Some(method)) => Some(format!("{class}::{method}")),
        (None, Some(method)) => Some(method),
        (_, None) => None,
    }
}

fn source_text(source: &[u8], span: Span) -> Cow<'_, str> {
    String::from_utf8_lossy(&source[span.start_offset() as usize..span.end_offset() as usize])
}

/// PHP's `dirname()`, restricted to the forward-slash-only relative paths used throughout this
/// module.
fn php_dirname(path: &str) -> String {
    match path.rfind('/') {
        None => ".".to_string(),
        Some(0) => "/".to_string(),
        Some(idx) => path[..idx].to_string(),
    }
}

fn decode_literal<'a>(raw: &'a [u8], value: Option<&'a [u8]>) -> Cow<'a, str> {
    String::from_utf8_lossy(value.unwrap_or(raw))
}

/// The equivalent of PHP's `addcslashes($value, "'\\")`, wrapped in single quotes.
fn quote_single(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(text.len() + 2);
    out.push('\'');
    for c in text.chars() {
        if c == '\'' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('\'');
    out
}

fn node_to_segment(expr: &Expression<'_>, context: &Context<'_>) -> String {
    match expr {
        Expression::Literal(Literal::String(string)) => decode_literal(string.raw, string.value).into_owned(),
        Expression::MagicConstant(MagicConstant::Directory(_)) => {
            let dir = php_dirname(context.current_file);
            if dir == "." { String::new() } else { format!("/{dir}") }
        }
        Expression::MagicConstant(MagicConstant::File(_)) => format!("/{}", context.current_file),
        _ if is_dirname_of_file_or_dir(expr) => {
            let mut depth = 0u32;
            let mut inner = expr;
            while let Expression::Call(Call::Function(call)) = inner {
                depth += 1;
                inner = call.argument_list.arguments.iter().next().expect("checked by is_dirname_of_file_or_dir").value();
            }
            // dirname(__DIR__) is equivalent to dirname(dirname(__FILE__)), so __DIR__ as the
            // innermost adds one extra level.
            if matches!(inner, Expression::MagicConstant(MagicConstant::Directory(_))) {
                depth += 1;
            }
            // `current_file` is repository-relative, so php_dirname sticks at '.' once it reaches
            // the top; the real __DIR__ is absolute and keeps climbing. Count the levels that go
            // past the repository root and emit them as '..' segments, so a path that leaves the
            // repository is not clamped onto an unrelated location inside it.
            let mut dir = context.current_file.to_string();
            let mut above_root = 0;
            for _ in 0..depth {
                if dir == "." { above_root += 1 } else { dir = php_dirname(&dir) }
            }
            if dir == "." { "/..".repeat(above_root) } else { format!("/{dir}") }
        }
        Expression::Access(Access::Property(PropertyAccess { object, property: ClassLikeMemberSelector::Identifier(id), .. }))
            if is_cfg_variable(object) =>
        {
            match id.value {
                b"dirroot" => context.notation.dirroot_segment().to_string(),
                b"libdir" => format!("{}/lib", context.notation.dirroot_segment()),
                // The bare repository root itself: zero path segments below it.
                b"root" => String::new(),
                // $CFG->admin is the admin directory *name* (default 'admin'), always written as
                // "$CFG->dirroot/$CFG->admin/...", so it contributes only the bare segment.
                b"admin" => "admin".to_string(),
                _ => format!("{{{}}}", source_text(context.file.contents.as_ref(), expr.span())),
            }
        }
        Expression::CompositeString(CompositeString::Interpolated(interpolated)) => {
            build_from_interpolated(interpolated, context)
        }
        Expression::ConstantAccess(ConstantAccess { name }) => {
            let name = identifier_name(name);
            if name == "DIRECTORY_SEPARATOR" { "/".to_string() } else { format!("{{{name}}}") }
        }
        _ => format!("{{{}}}", source_text(context.file.contents.as_ref(), expr.span())),
    }
}

fn build_from_parts(parts: &[&Expression<'_>], context: &Context<'_>) -> String {
    parts.iter().map(|part| node_to_segment(part, context)).collect()
}

/// `literal_value` (the plain text of a string literal), resolved as if it were
/// `__DIR__ . '/' . $literal_value` — relative to the current file's directory.
fn dirname_relative(literal_value: &str, context: &Context<'_>) -> String {
    let dir = php_dirname(context.current_file);
    if dir == "." { format!("/{literal_value}") } else { format!("/{dir}/{literal_value}") }
}

fn build_from_interpolated(interpolated: &InterpolatedString<'_>, context: &Context<'_>) -> String {
    let mut result = String::new();
    for part in interpolated.parts.iter() {
        match part {
            StringPart::Literal(l) => result.push_str(&decode_literal(l.raw, l.value)),
            StringPart::Expression(expr) => result.push_str(&node_to_segment(expr, context)),
            StringPart::BracedExpression(braced) => result.push_str(&node_to_segment(braced.expression, context)),
        }
    }
    result
}

/// The repository-root-relative path (no leading slash) that `expr` resolves to, along with its
/// glyph-notation rendering, if `expr` is a potential path expression.
fn extract_path(expr: &Expression<'_>, context: &Context<'_>) -> Option<(String, String)> {
    let repo_relative = if as_concat(expr).is_some() {
        let mut parts = Vec::new();
        flatten_concat(expr, &mut parts);
        let (anchor, rest) = parts.split_first().expect("a concat always flattens to at least two parts");
        PathNotation::normalise(&format!("{}{}", node_to_segment(anchor, context), build_from_parts(rest, context)))
    } else {
        match expr {
            Expression::CompositeString(CompositeString::Interpolated(interpolated)) => {
                PathNotation::normalise(&build_from_interpolated(interpolated, context))
            }
            // A bare $CFG->dirroot/libdir/root is not normalised, so a trailing slash from e.g.
            // "$CFG->dirroot . '/'" stays distinct from the bare value.
            Expression::Access(Access::Property(_)) => node_to_segment(expr, context),
            _ => return None,
        }
    };

    let glyph = context.notation.to_glyph(&repo_relative);
    Some((repo_relative.trim_start_matches('/').to_string(), glyph))
}

fn extract_separator(expr: &Expression<'_>) -> String {
    if as_concat(expr).is_some() {
        let mut parts = Vec::new();
        flatten_concat(expr, &mut parts);
        if parts.len() >= 2 {
            match parts[1] {
                Expression::Literal(Literal::String(s)) if s.value == Some(&b"/"[..]) => {
                    return "/".to_string();
                }
                Expression::ConstantAccess(ConstantAccess { name }) if identifier_name(name) == "DIRECTORY_SEPARATOR" => {
                    return "DIRECTORY_SEPARATOR".to_string();
                }
                _ => {}
            }
        }
        return String::new();
    }
    if let Expression::CompositeString(CompositeString::Interpolated(interpolated)) = expr {
        let parts = meaningful_parts(interpolated);
        if parts.len() >= 2
            && let StringPart::Literal(l) = parts[1]
            && l.value == Some(&b"/"[..])
        {
            return "/".to_string();
        }
    }
    String::new()
}

/// The faithful PHP expression for the part of a dirroot-rooted path after $CFG->dirroot, i.e.
/// the path relative to dirroot, ready to pass to core\component::from_mono_path().
///
/// Unlike the normalised path, this preserves the source exactly — in particular
/// DIRECTORY_SEPARATOR stays as the constant rather than being flattened to '/', and the leading
/// separator (if any) is included. Returns '' for paths not rooted at $CFG->dirroot (e.g.
/// $CFG->libdir, __DIR__); those are never variable-only and so never reach from_mono_path().
fn extract_mono_path_expr(expr: &Expression<'_>, context: &Context<'_>) -> String {
    let source = context.file.contents.as_ref();
    if as_concat(expr).is_some() {
        let mut parts = Vec::new();
        flatten_concat(expr, &mut parts);
        if parts.is_empty() || !is_dirroot_root(parts[0]) {
            return String::new();
        }
        return parts[1..].iter().map(|part| source_text(source, part.span()).into_owned()).collect::<Vec<_>>().join(" . ");
    }
    if let Expression::CompositeString(CompositeString::Interpolated(interpolated)) = expr {
        let parts = meaningful_parts(interpolated);
        if parts.is_empty() || !string_part_expr(parts[0]).is_some_and(is_cfg_dirroot) {
            return String::new();
        }
        return parts[1..]
            .iter()
            .map(|part| match part {
                StringPart::Literal(l) => quote_single(l.value.unwrap_or(l.raw)),
                StringPart::Expression(e) => source_text(source, e.span()).into_owned(),
                StringPart::BracedExpression(b) => source_text(source, b.expression.span()).into_owned(),
            })
            .collect::<Vec<_>>()
            .join(" . ");
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn top_level_expr<'arena>(arena: &'arena LocalArena, code: &str) -> &'arena Expression<'arena> {
        let source = format!("<?php {code};");
        let file = File::new(Cow::Borrowed(b"test.php"), FileType::Host, None, Cow::Owned(source.into_bytes()));
        let program = mago_syntax::parser::parse_file(arena, &file);
        program
            .statements
            .iter()
            .find_map(|statement| match statement {
                mago_syntax::cst::Statement::Expression(stmt) => Some(stmt.expression),
                _ => None,
            })
            .expect("expected an expression statement")
    }

    #[test]
    fn is_potential_path_dirroot_concat() {
        let arena = LocalArena::new();
        assert!(is_potential_path(top_level_expr(&arena, "$CFG->dirroot . 'lib.php'")));
    }

    #[test]
    fn is_potential_path_dir_magic_constant_concat() {
        let arena = LocalArena::new();
        assert!(is_potential_path(top_level_expr(&arena, "__DIR__ . 'lib.php'")));
    }

    #[test]
    fn is_potential_path_file_magic_constant_concat() {
        let arena = LocalArena::new();
        assert!(is_potential_path(top_level_expr(&arena, "__FILE__ . 'lib.php'")));
    }

    #[test]
    fn is_potential_path_unrelated_concat_is_false() {
        let arena = LocalArena::new();
        assert!(!is_potential_path(top_level_expr(&arena, "'/var/www/html/' . 'lib.php'")));
    }
}
