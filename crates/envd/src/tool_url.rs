//! App-owned internal URL resolver composition.

mod artifact;
mod attachment;
mod docs;
pub mod host;
pub(super) mod local;
mod mcp;
mod memory;
pub(super) mod ssh;
pub(super) mod vault;

use std::{fmt::Display, path::PathBuf, str, sync::Arc};

use omp_agent::SessionAuthority;
use omp_cache::github_cache::GithubCache;
use omp_core::{CowBytes, Str};
use omp_journal::blob::BlobStore;
use omp_tools::read::{
	Fault,
	conflicts::{ConflictRegistry, ConflictResolver},
	json_query::{apply_query, parse_query, path_to_query, render_value},
	resolver::{
		LineOffsetCache, Resolve, ResolverTable, ResourceCompletion, ResourceEntry, ResourceList,
		Scheme, SchemeEntry, fuzzy_score,
	},
	selector::ParsedSelector,
};

use super::{
	github_url::{GithubCredentialBridge, GithubResolver, GithubScheme},
	mcp::McpService,
	ssh::SshService,
	vault::VaultService,
};
use crate::{ContentResolver, HostResources};

#[derive(Clone, Copy, Debug)]
enum RegistryResource {
	Agent,
	History,
}

pub(super) struct RegistryResolver {
	resource:  RegistryResource,
	lines:     LineOffsetCache,
	authority: Option<Arc<dyn SessionAuthority>>,
}

impl RegistryResolver {
	fn new(resource: RegistryResource, authority: Option<Arc<dyn SessionAuthority>>) -> Self {
		Self { resource, lines: LineOffsetCache::default(), authority }
	}

	fn authority(&self) -> Result<&dyn SessionAuthority, Fault> {
		self.authority.as_deref().ok_or_else(|| Fault::Source {
			message: Str::new_static("No live session registry is bound."),
		})
	}
}

impl Resolve for RegistryResolver {
	async fn read<'a>(
		&'a self,
		resource: &'a str,
		selector: &'a ParsedSelector,
	) -> Result<CowBytes<'static>, Fault> {
		self.read_query(resource, None, selector).await
	}

	async fn read_query<'a>(
		&'a self,
		resource: &'a str,
		query: Option<&'a str>,
		selector: &'a ParsedSelector,
	) -> Result<CowBytes<'static>, Fault> {
		let authority = self.authority()?;
		let bytes = if matches!(self.resource, RegistryResource::History)
			&& resource.trim_matches('/').is_empty()
		{
			let rows = authority
				.list()
				.into_iter()
				.map(|endpoint| {
					serde_json::json!({
						"id": endpoint.id,
						"name": endpoint.name,
					})
				})
				.collect::<Vec<_>>();
			serde_json::to_vec(&rows).map_err(json_fault)?
		} else {
			let (base, path) = resource.split_once('/').unwrap_or((resource, ""));
			let endpoint = authority.lookup(base).ok_or_else(|| Fault::Source {
				message: Str::new(format!("Session `{base}` is not live.")),
			})?;
			let bytes = endpoint.snapshot.read().as_bytes().to_vec();
			match self.resource {
				RegistryResource::Agent => {
					if query.is_some() && !path.is_empty() {
						return Err(Fault::Invalid {
							message: Str::new_static("agent:// cannot combine path extraction with ?q=."),
						});
					}
					project_json(bytes, query, (!path.is_empty()).then_some(path))?
				},
				RegistryResource::History => render_history(resource, bytes)?,
			}
		};
		select_bytes(&self.lines, resource, CowBytes::from(bytes), selector)
	}

	async fn path(&self, _resource: &str) -> Result<Option<Str>, Fault> {
		Ok(None)
	}

	async fn list(
		&self,
		resource: &str,
		max_entries: usize,
		max_bytes: usize,
	) -> Result<ResourceList, Fault> {
		if !resource.trim_matches('/').is_empty() {
			return Err(Fault::Invalid {
				message: Str::new_static(
					"Session resource listing is supported only at the scheme root.",
				),
			});
		}
		let scheme = match self.resource {
			RegistryResource::Agent => "agent",
			RegistryResource::History => "history",
		};
		let mut entries = Vec::new();
		let mut bytes = 0usize;
		let mut truncated = false;
		for endpoint in self.authority()?.list() {
			let uri = format!("{scheme}://{}", endpoint.id);
			let entry_bytes = uri.len().saturating_add(endpoint.name.len());
			if entries.len() == max_entries || bytes.saturating_add(entry_bytes) > max_bytes {
				truncated = true;
				break;
			}
			bytes += entry_bytes;
			entries.push(ResourceEntry {
				uri:       Str::new(uri),
				name:      endpoint.name,
				directory: false,
				size:      0,
			});
		}
		Ok(ResourceList { entries, truncated })
	}

	async fn complete(
		&self,
		query: &str,
		max_results: usize,
	) -> Result<Vec<ResourceCompletion>, Fault> {
		let scheme = match self.resource {
			RegistryResource::Agent => "agent",
			RegistryResource::History => "history",
		};
		let mut matches = self
			.authority()?
			.list()
			.into_iter()
			.filter_map(|endpoint| {
				let score =
					fuzzy_score(query, &endpoint.id).or_else(|| fuzzy_score(query, &endpoint.name))?;
				Some(ResourceCompletion {
					value: Str::new(format!("{scheme}://{}", endpoint.id)),
					description: endpoint.name,
					score,
				})
			})
			.collect::<Vec<_>>();
		matches.sort_unstable_by(|left, right| {
			right
				.score
				.cmp(&left.score)
				.then_with(|| left.value.cmp(&right.value))
		});
		matches.truncate(max_results);
		Ok(matches)
	}
}

/// Constructor-owned resolver union used by the production read registry.
pub(super) enum UrlResolver {
	/// RPC host-owned generation-fenced resources.
	Host(host::HostUriResolver),
	/// Session artifacts by ordinal or durable digest.
	Artifact(artifact::ArtifactUrlResolver),
	/// Images from the latest projected user message.
	Attachment(attachment::AttachmentUrlResolver),
	/// Agent output and child artifacts.
	Agent(RegistryResolver),
	/// Read-only agent transcript index and bodies.
	History(RegistryResolver),
	/// Direct GitHub issue views.
	Issue(GithubResolver),
	/// Direct GitHub pull-request views and diffs.
	Pr(GithubResolver),
	/// Session-local scratch files.
	Local(local::LocalResolver),
	/// Active-session bounded memory projections.
	Memory(memory::MemoryUrlResolver),
	/// Configured native SSH hosts.
	Ssh(ssh::SshResolver),
	/// Configured local vaults.
	Vault(vault::VaultResolver),
	/// Resources exposed by mounted MCP servers.
	Mcp(mcp::McpUrlResolver),
	/// Composition-owned active content.
	Content(Arc<dyn ContentResolver>),
	/// Session-registered merge conflict regions.
	Conflict(ConflictResolver),
	/// Packaged harness documentation.
	Docs(docs::DocsResolver),
}

impl Resolve for UrlResolver {
	async fn read<'a>(
		&'a self,
		resource: &'a str,
		selector: &'a ParsedSelector,
	) -> Result<CowBytes<'static>, Fault> {
		match self {
			Self::Host(resolver) => resolver.read(resource, selector).await,
			Self::Artifact(resolver) => resolver.read(resource, selector).await,
			Self::Attachment(resolver) => resolver.read(resource, selector).await,
			Self::Agent(resolver) | Self::History(resolver) => resolver.read(resource, selector).await,
			Self::Issue(resolver) | Self::Pr(resolver) => resolver.read(resource, selector).await,
			Self::Local(resolver) => resolver.read(resource, selector).await,
			Self::Memory(resolver) => resolver.read(resource, selector).await,
			Self::Ssh(resolver) => resolver.read(resource, selector).await,
			Self::Vault(resolver) => resolver.read(resource, selector).await,
			Self::Mcp(resolver) => resolver.read(resource, selector).await,
			Self::Content(resolver) => resolver.read(resource, selector).await,
			Self::Conflict(resolver) => resolver.read(resource, selector).await,
			Self::Docs(resolver) => resolver.read(resource, selector).await,
		}
	}

	async fn read_query<'a>(
		&'a self,
		resource: &'a str,
		query: Option<&'a str>,
		selector: &'a ParsedSelector,
	) -> Result<CowBytes<'static>, Fault> {
		match self {
			Self::Host(resolver) => resolver.read_query(resource, query, selector).await,
			Self::Agent(resolver) | Self::History(resolver) => {
				resolver.read_query(resource, query, selector).await
			},
			Self::Issue(resolver) | Self::Pr(resolver) => {
				resolver.read_query(resource, query, selector).await
			},
			Self::Ssh(resolver) => resolver.read_query(resource, query, selector).await,
			Self::Vault(resolver) if query.is_some() => {
				resolver.read_query(resource, query, selector).await
			},
			Self::Content(resolver) => resolver.read_query(resource, query, selector).await,
			_ => self.read(resource, selector).await,
		}
	}

	async fn list(
		&self,
		resource: &str,
		max_entries: usize,
		max_bytes: usize,
	) -> Result<ResourceList, Fault> {
		match self {
			Self::Host(_) => Err(Fault::Invalid {
				message: Str::new_static("Host resources do not support listing."),
			}),
			Self::Artifact(resolver) => resolver.list(resource, max_entries, max_bytes).await,
			Self::Attachment(_) => Err(Fault::Invalid {
				message: Str::new_static("Attachment resources cannot be listed."),
			}),
			Self::Agent(resolver) | Self::History(resolver) => {
				resolver.list(resource, max_entries, max_bytes).await
			},
			Self::Local(resolver) => resolver.list(resource, max_entries, max_bytes).await,
			Self::Memory(resolver) => resolver.list(resource, max_entries, max_bytes).await,
			Self::Ssh(resolver) => resolver.list(resource, max_entries, max_bytes).await,
			Self::Vault(resolver) => resolver.list(resource, max_entries, max_bytes).await,
			Self::Mcp(_) => Err(Fault::Invalid {
				message: Str::new_static(
					"MCP resources are discovered through the mounted server device.",
				),
			}),
			Self::Content(resolver) => resolver.list(resource, max_entries, max_bytes).await,
			Self::Docs(resolver) => resolver.list(resource, max_entries, max_bytes).await,
			Self::Issue(_) | Self::Pr(_) => Err(Fault::Invalid {
				message: Str::new_static("GitHub list resources are read as Markdown."),
			}),
			Self::Conflict(_) => {
				Err(Fault::Invalid { message: Str::new_static("Conflict resources cannot be listed.") })
			},
		}
	}

	async fn path(&self, resource: &str) -> Result<Option<Str>, Fault> {
		match self {
			Self::Host(_) => Err(Fault::Invalid {
				message: Str::new_static("Host resources have no local materializable path."),
			}),
			Self::Agent(resolver) => resolver.path(resource).await,
			Self::Local(resolver) => resolver.path(resource).await,
			Self::Content(resolver) => resolver.path(resource).await,
			Self::Ssh(_) | Self::Vault(_) => Err(Fault::Invalid {
				message: Str::new_static(
					"Remote and vault resources have no local materializable path.",
				),
			}),
			Self::Mcp(_) => Err(Fault::Invalid {
				message: Str::new_static("MCP resources have no local materializable path."),
			}),
			_ => Err(Fault::Invalid {
				message: Str::new_static("This resource has no materializable path."),
			}),
		}
	}

	async fn complete(
		&self,
		query: &str,
		max_results: usize,
	) -> Result<Vec<ResourceCompletion>, Fault> {
		match self {
			Self::Host(_) | Self::Attachment(_) => Ok(Vec::new()),
			Self::Artifact(resolver) => resolver.complete(query, max_results).await,
			Self::Agent(resolver) | Self::History(resolver) => {
				resolver.complete(query, max_results).await
			},
			Self::Local(resolver) => resolver.complete(query, max_results).await,
			Self::Memory(resolver) => resolver.complete(query, max_results).await,
			Self::Ssh(resolver) => resolver.complete(query, max_results).await,
			Self::Vault(resolver) => resolver.complete(query, max_results).await,
			Self::Mcp(resolver) => resolver.complete(query, max_results).await,
			Self::Content(resolver) => resolver.complete(query, max_results).await,
			Self::Docs(resolver) => resolver.complete(query, max_results).await,
			Self::Issue(_) | Self::Pr(_) | Self::Conflict(_) => Ok(Vec::new()),
		}
	}
}

/// Builds the production internal URL table and shared conflict registry.
pub(super) fn production_url_resolvers(
	conflicts: Arc<ConflictRegistry>,
	blob_store: BlobStore,
	session_id: &str,
	sessions_dir: PathBuf,
	workspace_root: PathBuf,
	github_cache: Arc<GithubCache>,
	github_credentials: Arc<GithubCredentialBridge>,
	content: Vec<Arc<dyn ContentResolver>>,
	host_resources: Option<Arc<dyn HostResources>>,
	session_authority: Option<Arc<dyn SessionAuthority>>,
	mcp: Arc<McpService>,
	ssh: SshService,
	vault: VaultService,
) -> Arc<ResolverTable<UrlResolver>> {
	let mut builder = ResolverTable::builder();
	if let Some(resources) = host_resources.as_ref() {
		let _ = host::bind(resources);
	}
	builder
		.install_unknown_fallback(UrlResolver::Host(host::HostUriResolver::new(host_resources)))
		.expect("RPC host URL fallback is unique");
	if let Some(runtime) =
		omp_memory::RuntimeRegistry::lookup(session_id).filter(|runtime| runtime.is_active())
	{
		builder
			.register(
				SchemeEntry::new(Scheme::Memory, true, false, "bounded active Mnemopi memory")
					.with_capabilities(true, false, true),
				UrlResolver::Memory(memory::MemoryUrlResolver::new(runtime)),
			)
			.expect("memory URL resolver is unique");
	}
	builder
		.register(
			SchemeEntry::new(Scheme::Ssh, true, false, "configured native SSH/SFTP hosts")
				.with_capabilities(true, false, true)
				.with_stamp(false, 1),
			UrlResolver::Ssh(ssh::SshResolver::new(ssh)),
		)
		.expect("ssh URL resolver is unique");
	builder
		.register(
			SchemeEntry::new(Scheme::Vault, true, false, "configured symlink-confined vaults")
				.with_capabilities(true, false, true)
				.with_stamp(false, 1),
			UrlResolver::Vault(vault::VaultResolver::new(vault)),
		)
		.expect("vault URL resolver is unique");
	builder
		.register(
			SchemeEntry::new(Scheme::Mcp, true, false, "resources from mounted MCP servers")
				.with_capabilities(false, false, true)
				.with_whole_body(true),
			UrlResolver::Mcp(mcp::McpUrlResolver::new(mcp)),
		)
		.expect("mcp URL resolver is unique");
	builder
		.register(
			SchemeEntry::new(Scheme::Issue, true, false, "direct GitHub issues"),
			UrlResolver::Issue(GithubResolver::new(
				GithubScheme::Issue,
				workspace_root.clone(),
				Arc::clone(&github_cache),
				Arc::clone(&github_credentials),
			)),
		)
		.expect("issue URL resolver is unique");
	builder
		.register(
			SchemeEntry::new(Scheme::Pr, true, false, "direct GitHub pull requests and diffs"),
			UrlResolver::Pr(GithubResolver::new(
				GithubScheme::PullRequest,
				workspace_root,
				github_cache,
				github_credentials,
			)),
		)
		.expect("pr URL resolver is unique");
	builder
		.register(
			SchemeEntry::new(Scheme::Attachment, true, false, "latest user image attachments"),
			UrlResolver::Attachment(attachment::AttachmentUrlResolver::new(
				blob_store.clone(),
				session_id,
				session_authority.clone(),
			)),
		)
		.expect("attachment URL resolver is unique");
	builder
		.register(
			SchemeEntry::new(
				Scheme::Artifact,
				true,
				false,
				"session artifacts by ordinal or durable digest",
			)
			.with_capabilities(true, true, true),
			UrlResolver::Artifact(
				artifact::ArtifactUrlResolver::open(blob_store, session_id)
					.expect("artifact catalog opens with the environment blob store"),
			),
		)
		.expect("artifact URL resolver is unique");
	builder
		.register(
			SchemeEntry::new(Scheme::Local, false, false, "session-local scratch files")
				.with_capabilities(true, true, true),
			UrlResolver::Local(
				local::LocalResolver::open(sessions_dir.clone())
					.expect("canonical sessions directory can be created"),
			),
		)
		.expect("local URL resolver is unique");
	for resolver in content {
		builder
			.register(resolver.entry(), UrlResolver::Content(resolver))
			.expect("composition content URL resolver is unique");
	}
	builder
		.register(
			SchemeEntry::new(Scheme::Agent, true, false, "settled agent output and child artifacts")
				.with_capabilities(true, true, true),
			UrlResolver::Agent(RegistryResolver::new(
				RegistryResource::Agent,
				session_authority.clone(),
			)),
		)
		.expect("agent URL resolver is unique");
	builder
		.register(
			SchemeEntry::new(Scheme::History, true, false, "read-only agent transcript index")
				.with_capabilities(true, false, true),
			UrlResolver::History(RegistryResolver::new(RegistryResource::History, session_authority)),
		)
		.expect("history URL resolver is unique");
	builder
		.register(
			SchemeEntry::new(Scheme::Conflict, true, false, "registered merge conflict regions"),
			UrlResolver::Conflict(ConflictResolver::new((*conflicts).clone())),
		)
		.expect("conflict URL resolver is unique");
	builder
		.register(
			SchemeEntry::new(Scheme::Omp, true, false, "packaged OMP documentation")
				.with_capabilities(true, false, true),
			UrlResolver::Docs(docs::DocsResolver::default()),
		)
		.expect("omp URL resolver is unique");
	Arc::new(builder.build())
}
fn project_json(bytes: Vec<u8>, query: Option<&str>, path: Option<&str>) -> Result<Vec<u8>, Fault> {
	let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|source| {
		Fault::Invalid { message: Str::new(format!("Agent output is not valid JSON: {source}")) }
	})?;
	let query = if let Some(query) = query {
		let mut selected = None;
		for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
			if name == "q" {
				if selected.replace(value.into_owned()).is_some() {
					return Err(Fault::Invalid {
						message: Str::new_static("agent:// accepts exactly one ?q= value."),
					});
				}
			} else {
				return Err(Fault::Invalid {
					message: Str::new(format!("Unsupported agent:// query parameter '{name}'.")),
				});
			}
		}
		Str::new(selected.ok_or_else(|| Fault::Invalid {
			message: Str::new_static("agent:// query form requires a nonempty ?q= value."),
		})?)
	} else if let Some(path) = path {
		path_to_query(path).map_err(json_fault)?
	} else {
		return Ok(bytes);
	};
	if query.is_empty() {
		return Err(Fault::Invalid {
			message: Str::new_static("agent:// JSON query cannot be empty."),
		});
	}
	let tokens = parse_query(&query).map_err(json_fault)?;
	let selected = apply_query(&value, &tokens).map_err(json_fault)?;
	render_value(selected, 8 * 1024 * 1024)
		.map(|rendered| rendered.as_bytes().to_vec())
		.map_err(json_fault)
}

fn render_history(resource: &str, bytes: Vec<u8>) -> Result<Vec<u8>, Fault> {
	if resource.trim_matches('/').is_empty() {
		return Ok(bytes);
	}
	let text = str::from_utf8(&bytes).map_err(|_| Fault::Invalid {
		message: Str::new_static("Agent transcript is not UTF-8 text."),
	})?;
	let mut output = format!("# {} transcript\n\n", resource.trim_matches('/'));
	let mut rendered = 0usize;
	for line in text.lines().filter(|line| !line.trim().is_empty()) {
		let value: serde_json::Value =
			serde_json::from_str(line).map_err(|source| Fault::Invalid {
				message: Str::new(format!("Agent transcript contains invalid JSONL: {source}")),
			})?;
		let role = find_json_string(&value, "role");
		let content = find_json_string(&value, "text")
			.or_else(|| find_json_string(&value, "content"))
			.or_else(|| find_json_string(&value, "message"));
		if let Some(content) = content {
			output.push_str("## ");
			output.push_str(role.unwrap_or("event"));
			output.push_str("\n\n");
			output.push_str(content);
			output.push_str("\n\n");
			rendered += 1;
		}
	}
	if rendered == 0 {
		output.push_str("_Transcript contains no renderable message text._\n");
	}
	Ok(output.into_bytes())
}

fn find_json_string<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
	match value {
		serde_json::Value::Object(object) => object
			.get(key)
			.and_then(serde_json::Value::as_str)
			.or_else(|| {
				object
					.values()
					.find_map(|value| find_json_string(value, key))
			}),
		serde_json::Value::Array(values) => {
			values.iter().find_map(|value| find_json_string(value, key))
		},
		_ => None,
	}
}

fn json_fault(error: impl Display) -> Fault {
	Fault::Invalid { message: Str::new(error.to_string()) }
}

pub(super) fn select_bytes(
	lines: &LineOffsetCache,
	resource: &str,
	bytes: CowBytes<'static>,
	selector: &ParsedSelector,
) -> Result<CowBytes<'static>, Fault> {
	let ParsedSelector::Lines { ranges, .. } = selector else {
		return Ok(bytes);
	};
	if ranges.len() == 1 {
		return lines
			.slice(resource, &bytes, ranges[0])
			.map(CowBytes::into_owned)
			.map_err(|error| Fault::Invalid { message: Str::from(error.to_string()) });
	}
	let mut output = Vec::new();
	for range in ranges {
		let piece = lines
			.slice(resource, &bytes, *range)
			.map_err(|error| Fault::Invalid { message: Str::from(error.to_string()) })?;
		output.extend_from_slice(&piece);
	}
	Ok(CowBytes::from(output))
}
