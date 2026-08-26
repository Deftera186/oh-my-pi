//! Workspace procedural macros.
//!
//! Entry points live here; each macro's implementation owns a module. The
//! [`dom!`] declarative markup macro lowers component-builder calls for
//! `omp-tui`. [`cached`] provides fixed-capacity, per-thread memoization.

use proc_macro::TokenStream;

mod cached;
mod dom;
mod markup;
mod view;

/// Memoizes a synchronous function in a fixed-capacity per-thread cache.
///
/// The accepted arguments are `size = <integer>` (required), `result = <bool>`
/// (optional, default `false`), and `name = "IDENT"` (optional). With
/// `result = true`, only successful values are cached; errors are recomputed.
#[proc_macro_attribute]
pub fn cached(attributes: TokenStream, item: TokenStream) -> TokenStream {
	match cached::expand(attributes.into(), item.into()) {
		Ok(tokens) => tokens.into(),
		Err(error) => error.into_compile_error().into(),
	}
}

/// Builds one component tree from markup with child-level `for`, `if`, and
/// `match` control flow.
#[proc_macro]
pub fn dom(input: TokenStream) -> TokenStream {
	match dom::expand(input.into()) {
		Ok(tokens) => tokens.into(),
		Err(error) => error.into_compile_error().into(),
	}
}
/// Builds one typed renderer view tree from markup with child-level `for`,
/// `if`, and `match` control flow.
#[proc_macro]
pub fn view(input: TokenStream) -> TokenStream {
	match view::expand(input.into()) {
		Ok(tokens) => tokens.into(),
		Err(error) => error.into_compile_error().into(),
	}
}
