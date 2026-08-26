//! Reserved extension-management routing without prompt-sentence theft.

use std::ffi::OsString;

use omp_core::Str;

/// Classifies a documented-looking obsolete management invocation.
pub fn redirect(arguments: &[OsString]) -> Option<Str> {
	let positional = arguments
		.iter()
		.skip(1)
		.filter_map(|value| value.to_str())
		.filter(|value| !value.starts_with('-'))
		.collect::<Vec<_>>();
	let first = *positional.first()?;
	let second = positional.get(1).copied();
	let bare = second.is_none();
	let marketplace_action = first == "marketplace"
		&& second.is_some_and(|value| matches!(value, "add" | "remove" | "rm" | "update" | "list"));
	let plugin_action = matches!(first, "plugin" | "plugins" | "extensions")
		&& second.is_some_and(|value| {
			matches!(
				value,
				"list"
					| "install"
					| "uninstall"
					| "remove" | "upgrade"
					| "enable" | "disable"
					| "search"
			)
		});
	let qualified = positional.iter().skip(1).any(|value| value.contains('@'));
	let reserved = matches!(
		first,
		"plugin"
			| "plugins"
			| "extensions"
			| "marketplace"
			| "uninstall"
			| "upgrade"
			| "enable"
			| "disable"
	);
	if reserved && (bare || marketplace_action || plugin_action || qualified) {
		Some(Str::from(format!(
			"`omp {first}` is not a native command; use `omp ext` for extension management, or `omp \
			 print {first} …` to send it as a prompt"
		)))
	} else {
		None
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn redirects_management_but_preserves_sentences() {
		assert!(redirect(&["omp", "marketplace", "add", "repo"].map(OsString::from)).is_some());
		assert!(redirect(&["omp", "upgrade", "the", "dependencies"].map(OsString::from)).is_none());
		assert!(redirect(&["omp", "install", "name@marketplace"].map(OsString::from)).is_none());
	}
}
