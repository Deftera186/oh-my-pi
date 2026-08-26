//! `view!` markup lowering: lowers the shared markup grammar into typed
//! `omp_tools::render::view` element trees.
//!
//! Unlike [`crate::dom`], lowering is uniform: every tag becomes
//! `El::new(Tag::…)`, every attribute a typed `Prop`, and children push into
//! the parent regardless of component specialization. Typed specialization
//! happens on the consuming side when the serialized tree is parsed by the
//! presentation host.

use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{format_ident, quote};
use syn::LitStr;

use crate::markup::{Attr, AttrValue, Child, Control, Element, Parser};

/// Expands the `view!` body into typed element-builder calls.
pub fn expand(input: TokenStream2) -> syn::Result<TokenStream2> {
	let mut parser = Parser::new(input);
	let root = parser.element()?;
	if let Some(token) = parser.peek() {
		return Err(syn::Error::new(token.span(), "expected a single root element"));
	}
	lower_element(&root)
}

const VIEW: &str = "::omp_tools::render::view";

fn view_path() -> TokenStream2 {
	let path: syn::Path = syn::parse_str(VIEW).expect("view module path parses");
	quote!(#path)
}

/// Converts a kebab-case markup name into the matching CamelCase variant.
fn camel(name: &str, span: Span) -> syn::Result<proc_macro2::Ident> {
	let mut variant = String::with_capacity(name.len());
	for part in name.split('-') {
		let mut chars = part.chars();
		let Some(first) = chars.next() else {
			return Err(syn::Error::new(span, "empty name segment"));
		};
		variant.extend(first.to_uppercase());
		variant.push_str(chars.as_str());
	}
	Ok(format_ident!("{variant}", span = span))
}

/// Attributes whose bare or quoted literal values are theme tones.
fn is_tone_prop(name: &str) -> bool {
	matches!(name, "fg" | "bg" | "bc" | "color" | "annotation-color")
}

fn lower_element(element: &Element) -> syn::Result<TokenStream2> {
	let view = view_path();
	let mut constructed = if let Some(icon) = &element.name.icon {
		let icon = LitStr::new(icon, element.name.span);
		quote!(#view::El::icon(#icon))
	} else {
		let tag = camel(&element.name.text, element.name.span)?;
		quote!(#view::El::new(#view::Tag::#tag))
	};
	for attr in &element.attrs {
		constructed = lower_attr(constructed, attr)?;
	}
	if element.children.is_empty() {
		return Ok(constructed);
	}

	let builder = format_ident!("__omp_view_el", span = Span::mixed_site());
	let statements = lower_child_statements(&builder, &element.children)?;
	Ok(quote!({
		let mut #builder = #constructed;
		#statements
		#builder
	}))
}

fn lower_attr(output: TokenStream2, attr: &Attr) -> syn::Result<TokenStream2> {
	let view = view_path();
	let prop = camel(&attr.name, attr.span)?;
	let value = match &attr.value {
		AttrValue::Flag => quote!(true),
		AttrValue::Expr(expr) => quote!(#expr),
		AttrValue::String(literal) | AttrValue::Bare(literal) if is_tone_prop(&attr.name) => {
			let tone = camel(&literal.value(), literal.span())?;
			quote!(#view::Tone::#tone)
		},
		AttrValue::String(literal) | AttrValue::Bare(literal) => quote!(#literal),
	};
	Ok(quote!(#output.prop(#view::Prop::#prop, #value)))
}

fn lower_child_statements(
	builder: &proc_macro2::Ident,
	children: &[Child],
) -> syn::Result<TokenStream2> {
	let mut statements = TokenStream2::new();
	for child in children {
		statements.extend(lower_child_statement(builder, child)?);
	}
	Ok(statements)
}

fn lower_child_statement(builder: &proc_macro2::Ident, child: &Child) -> syn::Result<TokenStream2> {
	Ok(match child {
		Child::Element(element) => {
			let element = lower_element(element)?;
			quote!(#builder.push(#element);)
		},
		Child::String(text) => quote!(#builder.push_text(#text);),
		Child::Expr(expr) => quote!(#builder.push({ #expr });),
		Child::Control(control) => lower_control(builder, control)?,
	})
}

fn lower_control(builder: &proc_macro2::Ident, control: &Control) -> syn::Result<TokenStream2> {
	Ok(match control {
		Control::For(control) => {
			let head = &control.head;
			let body = lower_child_statements(builder, &control.body)?;
			quote!(#head { #body })
		},
		Control::If(control) => {
			let mut output = TokenStream2::new();
			for (index, branch) in control.branches.iter().enumerate() {
				let head = &branch.head;
				let body = lower_child_statements(builder, &branch.body)?;
				if index == 0 {
					output.extend(quote!(#head { #body }));
				} else {
					output.extend(quote!(else #head { #body }));
				}
			}
			if let Some(children) = &control.else_body {
				let body = lower_child_statements(builder, children)?;
				output.extend(quote!(else { #body }));
			}
			output
		},
		Control::Match(control) => {
			let head = &control.head;
			let mut arms = TokenStream2::new();
			for arm in &control.arms {
				let prefix = &arm.prefix;
				let body = lower_child_statements(builder, &arm.body)?;
				arms.extend(quote!(#prefix => { #body },));
			}
			quote!(#head { #arms })
		},
	})
}
