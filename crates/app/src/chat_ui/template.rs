//! Native Markdown command rendering through the shared scribe engine.

use std::sync::LazyLock;

use omp_core::Str;
use omp_scribe::{Engine, Props, Template};

/// Parsed arguments supplied to a native Markdown command template.
#[derive(Clone, Copy, Debug)]
pub struct TemplateArguments<'a> {
	/// Original argument tail, with quoting preserved.
	pub raw:   &'a str,
	/// Tokenized arguments after quote grouping.
	pub words: &'a [Str],
}

fn engine() -> &'static Engine {
	static ENGINE: LazyLock<Engine> = LazyLock::new(Engine::new);
	&ENGINE
}

/// Compiles one user-authored command template with the approved helper
/// vocabulary.
pub fn compile(template: &str) -> miette::Result<Template> {
	engine()
		.compile_owned(Str::new_static("command"), template)
		.map_err(|error| miette::miette!("command template compilation failed: {error}"))
}

/// Reports whether a compiled command template consumes raw or tokenized
/// arguments.
pub fn references_arguments(template: &Template) -> bool {
	template
		.referenced_keys()
		.any(|key| matches!(key, "args" | "arguments"))
}

/// Renders one native command template with scribe builtins.
pub fn render(template: &str, arguments: TemplateArguments<'_>) -> miette::Result<Str> {
	let template = compile(template)?;
	render_compiled(&template, arguments)
}

/// Renders one already-compiled native command template.
pub fn render_compiled(
	template: &Template,
	arguments: TemplateArguments<'_>,
) -> miette::Result<Str> {
	let mut props = Props::new();
	props.set("args", arguments.raw.to_owned());
	props.set("arguments", arguments.words.iter().cloned().collect::<Vec<_>>());
	template
		.render_str(engine(), &props)
		.map_err(|error| miette::miette!("command template rendering failed: {error}"))
}

#[cfg(test)]
mod tests {
	use omp_core::sf;

	use super::*;

	#[test]
	fn renders_only_the_native_helper_vocabulary() {
		let words = [sf!("one"), sf!("two")];
		let rendered = render(
			"{{ args }}|{{ arguments | join(\",\") }}|{% if arguments %}yes{% else %}no{% endif \
			 %}|{% codeblock \"rs\" %}fn main() {}{% endcodeblock %}|{% xml \"note\" %}{{ \"<ok>\" | \
			 escape_xml }}{% endxml %}",
			TemplateArguments { raw: "\"one\" two", words: &words },
		)
		.expect("render");
		assert_eq!(
			rendered,
			"\"one\" two|one,two|yes|```rs\nfn main() {}\n```|<note>\n&lt;ok&gt;\n</note>",
		);
	}

	#[test]
	fn referenced_keys_detect_argument_consumption() {
		assert!(references_arguments(&compile("{{ arguments[0] }}").unwrap()));
		assert!(references_arguments(&compile("{{ args }}").unwrap()));
		assert!(!references_arguments(&compile("literal").unwrap()));
	}
}
