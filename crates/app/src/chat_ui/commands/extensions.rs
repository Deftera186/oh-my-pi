use omp_core::Str;

mod inspector_model;

pub(crate) use inspector_model::{build_inspector_snapshot_from_declarations, snapshot_live_mcp};

use super::{ConfigScope, ExtensionRequest, MarketplaceRequest, PluginRequest, command};

command!(extensions, 132, "extensions", icon: ExtensionCommand, ["status"], "Inspect discovered extensions and live MCP catalogs", [Workspace, Owner], false, typed("", [], parse_extensions) => |host, _parsed| host.extensions(ExtensionRequest::Inspect));
command!(marketplace, 640, "marketplace", icon: Cart, [], "Manage marketplace plugin sources and installed plugins", [Workspace, Owner], false, typed("<subcommand>", ["add <source>", "remove <name>", "update [name]", "list", "discover [marketplace]", "install [--force] [name@marketplace]", "uninstall [name@marketplace]", "installed", "upgrade [name@marketplace]", "help"], parse_marketplace) => |host, request| host.extensions(ExtensionRequest::Marketplace(request)));
command!(plugins, 650, "plugins", icon: Package, [], "View and manage installed native plugins", [Workspace, Owner], false, typed("[list|enable|disable]", ["list", "enable", "disable", "--scope"], parse_plugins) => |host, request| host.extensions(ExtensionRequest::Plugins(request)));
command!(reload_plugins, 660, "reload-plugins", icon: Refresh, [], "Reload all native plugins, commands, skills, tools, and agents", [Workspace, Owner], false, none => |host| host.extensions(ExtensionRequest::Reload));

pub(super) fn parse_extensions(raw: &str) -> miette::Result<()> {
	if raw.trim().is_empty() {
		Ok(())
	} else {
		Err(miette::miette!("usage: /extensions"))
	}
}

fn parse_marketplace(raw: &str) -> miette::Result<MarketplaceRequest> {
	let mut words = raw.split_whitespace();
	let operation = words.next().unwrap_or("list");
	match operation {
		"list" => none(words, MarketplaceRequest::List),
		"installed" => none(words, MarketplaceRequest::Installed),
		"help" => none(words, MarketplaceRequest::Help),
		"add" => one(words, MarketplaceRequest::Add),
		"remove" | "rm" => one(words, MarketplaceRequest::Remove),
		"update" => optional_one(words, MarketplaceRequest::Update),
		"discover" => optional_one(words, MarketplaceRequest::Discover),
		"install" => parse_package(words, true, |spec, scope, force| MarketplaceRequest::Install {
			spec,
			scope,
			force,
		}),
		"uninstall" => {
			parse_package(words, false, |spec, scope, _| MarketplaceRequest::Uninstall { spec, scope })
		},
		"upgrade" => parse_upgrade(words),
		_ => Err(usage()),
	}
}

fn parse_plugins(raw: &str) -> miette::Result<PluginRequest> {
	let mut words = raw.split_whitespace();
	match words.next().unwrap_or("list") {
		"list" if words.next().is_none() => Ok(PluginRequest::List),
		"enable" => parse_plugin_toggle(words, true),
		"disable" => parse_plugin_toggle(words, false),
		_ => Err(plugin_usage()),
	}
}

fn parse_plugin_toggle<'a>(
	words: impl Iterator<Item = &'a str>,
	enabled: bool,
) -> miette::Result<PluginRequest> {
	let mut scope = ConfigScope::User;
	let mut id = None;
	let mut words = words.peekable();
	while let Some(word) = words.next() {
		match word {
			"--scope" => {
				scope = match words.next() {
					Some("user") => ConfigScope::User,
					Some("project") => ConfigScope::Project,
					_ => return Err(miette::miette!("--scope must be `user` or `project`")),
				};
			},
			value if value.starts_with("--") => {
				return Err(miette::miette!("unknown plugin option `{value}`"));
			},
			value if id.is_none() => id = Some(Str::new(value)),
			_ => return Err(plugin_usage()),
		}
	}
	let id = id.ok_or_else(plugin_usage)?;
	Ok(if enabled {
		PluginRequest::Enable { id, scope }
	} else {
		PluginRequest::Disable { id, scope }
	})
}

fn plugin_usage() -> miette::Report {
	miette::miette!(
		"usage: /plugins [list|enable [--scope user|project] <name>|disable [--scope user|project] \
		 <name>]"
	)
}

fn parse_package<'a>(
	words: impl Iterator<Item = &'a str>,
	allow_force: bool,
	build: impl FnOnce(Str, ConfigScope, bool) -> MarketplaceRequest,
) -> miette::Result<MarketplaceRequest> {
	let mut scope = ConfigScope::User;
	let mut force = false;
	let mut spec = None;
	let mut words = words.peekable();
	while let Some(word) = words.next() {
		match word {
			"--force" if allow_force => force = true,
			"--scope" => {
				scope = match words.next() {
					Some("user") => ConfigScope::User,
					Some("project") => ConfigScope::Project,
					_ => return Err(miette::miette!("--scope must be `user` or `project`")),
				};
			},
			value if value.starts_with("--") => {
				return Err(miette::miette!("unknown marketplace option `{value}`"));
			},
			value if spec.is_none() => spec = Some(Str::new(value)),
			_ => return Err(usage()),
		}
	}
	let spec = spec.ok_or_else(usage)?;
	if !spec.contains('@') {
		return Err(miette::miette!("package must use `name@marketplace` syntax"));
	}
	Ok(build(spec, scope, force))
}

fn parse_upgrade<'a>(words: impl Iterator<Item = &'a str>) -> miette::Result<MarketplaceRequest> {
	let mut scope = ConfigScope::User;
	let mut spec = None;
	let mut words = words.peekable();
	while let Some(word) = words.next() {
		match word {
			"--scope" => {
				scope = match words.next() {
					Some("user") => ConfigScope::User,
					Some("project") => ConfigScope::Project,
					_ => return Err(miette::miette!("--scope must be `user` or `project`")),
				};
			},
			value if value.starts_with("--") || spec.is_some() => return Err(usage()),
			value => spec = Some(Str::new(value)),
		}
	}
	if spec.as_ref().is_some_and(|spec| !spec.contains('@')) {
		return Err(miette::miette!("package must use `name@marketplace` syntax"));
	}
	Ok(MarketplaceRequest::Upgrade { spec, scope })
}

fn none<'a>(
	mut words: impl Iterator<Item = &'a str>,
	request: MarketplaceRequest,
) -> miette::Result<MarketplaceRequest> {
	if words.next().is_none() {
		Ok(request)
	} else {
		Err(usage())
	}
}

fn one<'a, T>(
	mut words: impl Iterator<Item = &'a str>,
	build: impl FnOnce(Str) -> T,
) -> miette::Result<T> {
	let value = words.next().ok_or_else(usage)?;
	if words.next().is_some() {
		Err(usage())
	} else {
		Ok(build(Str::new(value)))
	}
}

fn optional_one<'a, T>(
	mut words: impl Iterator<Item = &'a str>,
	build: impl FnOnce(Option<Str>) -> T,
) -> miette::Result<T> {
	let value = words.next().map(Str::new);
	if words.next().is_some() {
		Err(usage())
	} else {
		Ok(build(value))
	}
}

fn usage() -> miette::Report {
	miette::miette!(
		"usage: /marketplace \
		 add|remove|update|list|discover|install|uninstall|installed|upgrade|help"
	)
}
#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn extensions_accepts_only_an_empty_argument_tail() {
		assert!(parse_extensions("").is_ok());
		assert!(parse_extensions("  \t").is_ok());
		assert!(parse_extensions("list").is_err());
		assert!(parse_extensions("--json").is_err());
	}
	#[test]
	fn marketplace_install_accepts_force_and_scope_in_any_order() {
		assert_eq!(
			parse_marketplace("install --scope project package@index --force").unwrap(),
			MarketplaceRequest::Install {
				spec:  Str::new_static("package@index"),
				scope: ConfigScope::Project,
				force: true,
			}
		);
		assert_eq!(
			parse_marketplace("install --force package@index --scope user").unwrap(),
			MarketplaceRequest::Install {
				spec:  Str::new_static("package@index"),
				scope: ConfigScope::User,
				force: true,
			}
		);
	}

	#[test]
	fn plugin_toggle_accepts_scoped_identity() {
		assert_eq!(
			parse_plugins("disable package@index --scope project").unwrap(),
			PluginRequest::Disable {
				id:    Str::new_static("package@index"),
				scope: ConfigScope::Project,
			}
		);
		assert_eq!(
			parse_plugins("enable --scope user package@index").unwrap(),
			PluginRequest::Enable {
				id:    Str::new_static("package@index"),
				scope: ConfigScope::User,
			}
		);
	}
}
