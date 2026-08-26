//! `model-gate` / `model-table`: model-name hardcoding inside
//! `crates/inference`. One family, one module: the model-id matcher, the
//! finding, and both position rules live here.
//!
//! Inference must branch on catalog data (capabilities, route policy, tier
//! metadata), never on model-name strings — name predicates rot the moment a
//! provider ships a new slug (`gpt-` misses `o3`, `contains("pro")` matches
//! anything). The rules:
//!
//! - `model-gate`: a string predicate (`starts_with`, `contains`,
//!   `match_indices`, `eq_ignore_ascii_case`, `strip_prefix`, …) or an
//!   `==`/`!=` comparison whose literal argument looks like a model id.
//! - `model-table`: an array/slice literal enumerating three or more
//!   model-like ids — an ad-hoc model table that belongs in catalog data.
//!
//! Scope: `crates/inference/src` only, excluding `#[cfg(test)]` modules and
//! `#[test]` functions — tests and fixtures legitimately name real models.
//! Literals that cannot be wire model ids (whitespace, uppercase, URLs,
//! hosts) never match, so codec registry keys, header names, and user-agent
//! fingerprints stay silent.

use std::ops::Range;

use ra_ap_syntax::{
	AstToken, SyntaxNode,
	ast::{self, AstNode, HasArgList, HasAttrs},
};

use crate::{
	fix::PathFix,
	lint::{Diagnosis, FileContext, Lint, RealtimeSink},
};

/// The `model-gate` lint; no configuration.
pub struct ModelGate;
/// The `model-table` lint; no configuration.
pub struct ModelTable;

/// Directories the family polices. Extend deliberately; the point is model
/// *behavior* crates, not UI or data crates.
const SCOPED_DIRS: &[&str] = &["crates/inference/src"];

/// String methods that turn a model-name literal into a behavior gate.
const PREDICATE_METHODS: &[&str] = &[
	"starts_with",
	"ends_with",
	"contains",
	"match_indices",
	"eq_ignore_ascii_case",
	"strip_prefix",
	"strip_suffix",
	"find",
	"rfind",
];

/// Model-family tokens. A literal is model-like when any `-`/`.`/`:`-separated
/// token equals one of these, or is one followed by digits (`qwen3`, `glm4`).
const FAMILIES: &[&str] = &[
	"gpt", "chatgpt", "claude", "gemini", "gemma", "glm", "grok", "qwen", "qwq", "deepseek",
	"kimi", "minimax", "llama", "mistral", "mixtral", "codestral", "sonnet", "opus", "haiku",
	"whisper", "o1", "o3", "o4",
];

/// Model-variant words that gate behavior only as the *entire* literal
/// (`model.contains("flash")`); as tokens they are far too generic.
///
/// `thinking` is deliberately absent: recovery code legitimately matches
/// ```` ```thinking ```` reasoning-fence keywords.
const VARIANT_WORDS: &[&str] = &["pro", "flash", "mini", "nano", "turbo", "preview"];
/// Whole literals naming local-server *software*, not models: endpoint
/// detection may match these in URLs (`base_url.contains("llama")` means
/// llama.cpp). Longer literals (`llama-3`) still match via family tokens.
const SOFTWARE_NAMES: &[&str] = &["llama", "ollama", "llamacpp", "lmstudio", "litellm", "vllm"];

/// One hardcoded model-name use.
pub struct Finding {
	span:    Range<usize>,
	message: String,
}

impl Diagnosis for Finding {
	fn span(&self) -> Range<usize> {
		self.span.clone()
	}

	fn message(&self) -> String {
		self.message.clone()
	}

	fn autofixable(&self) -> bool {
		false
	}

	fn fix(self) -> Option<PathFix> {
		None
	}
}

impl Lint for ModelGate {
	type Instance = Finding;

	const NAME: &'static str = "model-gate";

	fn detect(&self, ctx: &FileContext<'_>, sink: &mut RealtimeSink<'_, Finding>) {
		if !in_scope(ctx) {
			return;
		}
		for node in ctx.tree.syntax().descendants() {
			if let Some(call) = ast::MethodCallExpr::cast(node.clone()) {
				let Some(name) = call.name_ref() else { continue };
				if !PREDICATE_METHODS.contains(&&*name.text()) {
					continue;
				}
				let Some(args) = call.arg_list() else { continue };
				for arg in args.args() {
					self.flag(&arg, "argument to a string predicate", sink);
				}
			} else if let Some(bin) = ast::BinExpr::cast(node) {
				let comparison = matches!(
					bin.op_kind(),
					Some(ast::BinaryOp::CmpOp(ast::CmpOp::Eq { .. }))
				);
				if !comparison {
					continue;
				}
				for side in [bin.lhs(), bin.rhs()].into_iter().flatten() {
					self.flag(&side, "equality comparison", sink);
				}
			}
		}
	}
}

impl ModelGate {
	fn flag(&self, expr: &ast::Expr, position: &str, sink: &mut RealtimeSink<'_, Finding>) {
		let Some(value) = string_literal(expr) else {
			return;
		};
		if !is_model_name(&value) || in_test_code(expr.syntax()) {
			return;
		}
		let range = expr.syntax().text_range();
		sink.push(Finding {
			span:    range.start().into()..range.end().into(),
			message: format!(
				"model-name `\"{value}\"` as {position} gates behavior on a model id; move the \
				 mapping to crates/catalog data"
			),
		});
	}
}

impl Lint for ModelTable {
	type Instance = Finding;

	const NAME: &'static str = "model-table";

	fn detect(&self, ctx: &FileContext<'_>, sink: &mut RealtimeSink<'_, Finding>) {
		if !in_scope(ctx) {
			return;
		}
		for array in ctx
			.tree
			.syntax()
			.descendants()
			.filter_map(ast::ArrayExpr::cast)
		{
			let model_like = array
				.exprs()
				.filter_map(|expr| string_literal(&expr))
				.filter(|value| is_model_name(value))
				.count();
			if model_like < 3 || in_test_code(array.syntax()) {
				continue;
			}
			let range = array.syntax().text_range();
			sink.push(Finding {
				span:    range.start().into()..range.end().into(),
				message: format!(
					"array enumerates {model_like} model-like ids; hardcoded model tables belong \
					 in crates/catalog data"
				),
			});
		}
	}
}

/// Whether this file is inside a policed directory.
fn in_scope(ctx: &FileContext<'_>) -> bool {
	let path = ctx.path.to_string_lossy();
	SCOPED_DIRS.iter().any(|dir| path.contains(dir))
}

/// Unescaped-enough text of a plain or raw string literal expression.
///
/// Escape sequences are left verbatim: model ids never contain them, and a
/// literal with backslashes simply fails the matcher.
fn string_literal(expr: &ast::Expr) -> Option<String> {
	let ast::Expr::Literal(literal) = expr else {
		return None;
	};
	let ast::LiteralKind::String(token) = literal.kind() else {
		return None;
	};
	let text = token.text();
	let text = text.strip_prefix('r').unwrap_or(text);
	let text = text.trim_matches('#');
	Some(text.strip_prefix('"')?.strip_suffix('"')?.to_string())
}

/// Whether a literal plausibly names a model (family token or whole-literal
/// variant word), after rejecting shapes that cannot be wire model ids.
fn is_model_name(value: &str) -> bool {
	let value = value.trim();
	if value.len() < 2 || value.len() > 64 {
		return false;
	}
	// Wire model ids are lowercase tokens: prose, enum fragments
	// (`MODEL_PROVIDER_GEMINI`), user agents, and headers all wash out here.
	if value
		.chars()
		.any(|c| c.is_whitespace() || c.is_uppercase() || matches!(c, ',' | '(' | ')' | '=' | '{' | '}'))
	{
		return false;
	}
	// URLs and hosts (`https://…`, `chatgpt.com`) are endpoints, not models.
	if value.contains("://")
		|| value
			.rsplit_once('.')
			.is_some_and(|(_, tld)| ["com", "io", "ai", "net", "org", "dev", "app"].contains(&tld))
	{
		return false;
	}
	if VARIANT_WORDS.contains(&value) {
		return true;
	}
	if SOFTWARE_NAMES.contains(&value) {
		return false;
	}
	value
		.split(|c: char| !c.is_ascii_alphanumeric())
		.filter(|token| !token.is_empty())
		.any(|token| {
			FAMILIES.iter().any(|family| {
				token == *family
					|| (token.len() > family.len()
						&& token.starts_with(family)
						&& token[family.len()..].bytes().all(|b| b.is_ascii_digit()))
			})
		})
}

/// Whether `node` sits inside a `#[test]` function or `#[cfg(test)]` module.
fn in_test_code(node: &SyntaxNode) -> bool {
	node.ancestors().any(|ancestor| {
		if let Some(function) = ast::Fn::cast(ancestor.clone()) {
			has_test_attr(function.attrs())
		} else if let Some(module) = ast::Module::cast(ancestor) {
			has_test_attr(module.attrs())
		} else {
			false
		}
	})
}

/// Whether any attribute is `#[test]`, `#[…::test]`, or a `cfg(test…)` gate.
fn has_test_attr(mut attrs: impl Iterator<Item = ast::Attr>) -> bool {
	attrs.any(|attr| {
		let text = attr.syntax().text().to_string();
		text.ends_with("test]") || text.contains("cfg(test") || text.contains("cfg(all(test")
	})
}
