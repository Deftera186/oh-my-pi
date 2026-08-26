//! Supervised named-tab browser daemon over `omp-webview` automation.

use std::{collections::HashMap, path::PathBuf, sync::Arc, thread, time::Duration};

use async_trait::async_trait;
use flume::Receiver;
use omp_core::{ArtifactUrl, Str, sf};
use omp_settings::BrowserSettings;
use omp_tools::browser::{Action, BrowserHost, Fault, Params, Payload, RunOperation};
use omp_webview::{
	Engine, FrameConfig, SurfaceKind, WebView, WebViewBuilder, WindowConfig,
	automation::{ExtractFormat, ObserveOptions, Selector},
};
use serde_json::{Value, json};

use crate::blobs::{BlobError, BlobHost};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_TIMEOUT: Duration = Duration::from_mins(5);

enum Request {
	Execute { params: Params, reply: flume::Sender<Result<Payload, Fault>> },
	Restart { headless: bool, reply: flume::Sender<Result<(), Fault>> },
}

/// Process-local browser supervisor. One actor owns every non-`Send` webview
/// handle and tears the complete tab set down when its request channel closes.
pub(crate) struct BrowserDaemon {
	requests: flume::Sender<Request>,
}

impl BrowserDaemon {
	/// Starts one daemon actor with content-addressed artifact storage and its
	/// initial typed surface-mode projection.
	pub(crate) fn start(blobs: BlobHost, settings: BrowserSettings) -> Arc<Self> {
		let (requests, receiver) = flume::unbounded::<Request>();
		thread::Builder::new()
			.name("omp-browser-daemon".to_owned())
			.spawn(move || run(receiver, blobs, settings.headless))
			.expect("browser daemon actor starts");
		Arc::new(Self { requests })
	}
}

#[async_trait]
impl BrowserHost for BrowserDaemon {
	async fn execute(&self, params: Params) -> Result<Payload, Fault> {
		let (reply, response) = flume::bounded(1);
		self
			.requests
			.send_async(Request::Execute { params, reply })
			.await
			.map_err(|_| daemon_closed())?;
		response.recv_async().await.map_err(|_| daemon_closed())?
	}

	async fn restart_for_mode_change(&self, headless: bool) -> Result<(), Fault> {
		let (reply, response) = flume::bounded(1);
		self
			.requests
			.send_async(Request::Restart { headless, reply })
			.await
			.map_err(|_| daemon_closed())?;
		response.recv_async().await.map_err(|_| daemon_closed())?
	}
}

fn run(receiver: Receiver<Request>, blobs: BlobHost, mut headless: bool) {
	let mut tabs = HashMap::<Str, WebView>::new();
	while let Ok(request) = receiver.recv() {
		match request {
			Request::Execute { params, reply } => {
				let result = execute(&mut tabs, &blobs, headless, params);
				let _ = reply.send(result);
			},
			Request::Restart { headless: next, reply } => {
				tabs.clear();
				headless = next;
				let _ = reply.send(Ok(()));
			},
		}
	}
}

fn execute(
	tabs: &mut HashMap<Str, WebView>,
	blobs: &BlobHost,
	headless: bool,
	params: Params,
) -> Result<Payload, Fault> {
	let name = params.name.clone().unwrap_or_else(|| sf!("main"));
	match params.action {
		Action::Open => open(tabs, name, headless, params),
		Action::Close => {
			if params.all {
				tabs.clear();
			} else if tabs.remove(&name).is_none() {
				return Err(not_found(&name));
			}
			Ok(Payload {
				action: Action::Close,
				name,
				url: None,
				title: None,
				result: Some(json!({ "remaining_tabs": tabs.len() })),
				artifacts: Vec::new(),
			})
		},
		Action::Run => run_tab(tabs, blobs, name, params),
	}
}

fn open(
	tabs: &mut HashMap<Str, WebView>,
	name: Str,
	headless: bool,
	params: Params,
) -> Result<Payload, Fault> {
	let surface = if headless {
		SurfaceKind::Frames
	} else {
		SurfaceKind::Window
	};
	let engine = Engine::find(surface).map_err(webview_fault)?;
	let mut builder = WebViewBuilder::new(engine).incognito(true);
	if let Some(url) = params.url.as_ref() {
		builder = builder.url(url.clone());
	}
	let width = params.width.unwrap_or(1280).clamp(320, 4096);
	let height = params.height.unwrap_or(800).clamp(240, 4096);
	let view = if headless {
		builder
			.build_frames(FrameConfig {
				width,
				height,
				scale: params.scale.unwrap_or(1.0).clamp(0.5, 4.0),
				..FrameConfig::default()
			})
			.map_err(webview_fault)?
	} else {
		builder
			.build_window(WindowConfig { width, height })
			.map_err(webview_fault)?
	};
	let timeout = timeout(&params);
	view
		.automation()
		.wait_for_navigation(timeout)
		.map_err(webview_fault)?;
	let url = view.url();
	let title = view.title();
	tabs.insert(name.clone(), view);
	Ok(Payload {
		action: Action::Open,
		name,
		url: Some(url),
		title: Some(title),
		result: None,
		artifacts: Vec::new(),
	})
}

fn run_tab(
	tabs: &mut HashMap<Str, WebView>,
	blobs: &BlobHost,
	name: Str,
	params: Params,
) -> Result<Payload, Fault> {
	let view = tabs.get(&name).ok_or_else(|| not_found(&name))?;
	let tab = view.automation();
	let timeout = timeout(&params);
	if let Some(url) = params.url.as_ref() {
		tab.goto(url, timeout).map_err(webview_fault)?;
	}
	let operation = params
		.operation
		.or_else(|| params.code.as_ref().map(|_| RunOperation::Evaluate));
	let operation = operation.ok_or_else(|| invalid("run requires `operation` or `code`"))?;
	let mut artifacts = Vec::new();
	let result = match operation {
		RunOperation::Evaluate => tab
			.evaluate(required(params.code.as_deref(), "code")?, timeout)
			.map_err(webview_fault)?,
		RunOperation::Observe => {
			let observation = tab
				.document()
				.observe(ObserveOptions::default())
				.map_err(webview_fault)?;
			json!({
				"url": observation.url,
				"title": observation.title,
				"text": observation.text,
				"truncated": observation.truncated,
				"elements": observation.elements.into_iter().map(|element| json!({
					"id": element.id,
					"ref": element.reference,
					"role": element.role,
					"name": element.name,
					"value": element.value,
					"bounds": element.bounds,
					"visible": element.visible,
				})).collect::<Vec<_>>(),
			})
		},
		RunOperation::AriaSnapshot => Value::String(
			tab.document()
				.aria_snapshot(selector(params.selector.as_deref())?)
				.map_err(webview_fault)?
				.to_string(),
		),
		RunOperation::Screenshot => {
			let screenshot = tab
				.screenshot(selector(params.selector.as_deref())?, params.full_page, timeout)
				.map_err(webview_fault)?;
			let id = blobs.put(&screenshot.data).map_err(blob_fault)?;
			let url = Str::new(ArtifactUrl::from_digest(id.hash).as_str());
			artifacts.push(url.clone());
			json!({ "artifact": url, "bytes": id.size, "clip": screenshot.clip })
		},
		RunOperation::ExtractText => Value::String(
			tab.extract(ExtractFormat::Text)
				.map_err(webview_fault)?
				.to_string(),
		),
		RunOperation::ExtractHtml => Value::String(
			tab.extract(ExtractFormat::Html)
				.map_err(webview_fault)?
				.to_string(),
		),
		RunOperation::Click => {
			element(view, &params)?.click().map_err(webview_fault)?;
			Value::Bool(true)
		},
		RunOperation::Type => {
			element(view, &params)?
				.type_text(required(params.value.as_deref(), "value")?)
				.map_err(webview_fault)?;
			Value::Bool(true)
		},
		RunOperation::Fill => {
			element(view, &params)?
				.fill(required(params.value.as_deref(), "value")?)
				.map_err(webview_fault)?;
			Value::Bool(true)
		},
		RunOperation::Select => {
			element(view, &params)?
				.select(
					params
						.values
						.as_deref()
						.ok_or_else(|| invalid("select requires `values`"))?,
				)
				.map_err(webview_fault)?;
			Value::Bool(true)
		},
		RunOperation::Press => {
			element(view, &params)?
				.press(required(params.value.as_deref(), "value")?)
				.map_err(webview_fault)?;
			Value::Bool(true)
		},
		RunOperation::ScrollIntoView => {
			element(view, &params)?
				.scroll_into_view()
				.map_err(webview_fault)?;
			Value::Bool(true)
		},
		RunOperation::Drag => {
			let source = element(view, &params)?;
			let target = view
				.automation()
				.document()
				.resolve(
					Selector::parse(required(params.target.as_deref(), "target")?)
						.map_err(webview_fault)?,
				)
				.map_err(webview_fault)?;
			source.drag_to(&target).map_err(webview_fault)?;
			Value::Bool(true)
		},
		RunOperation::Upload => {
			let paths = params
				.values
				.as_deref()
				.ok_or_else(|| invalid("upload requires `values`"))?;
			let paths = paths
				.iter()
				.map(|path| PathBuf::from(path.as_str()))
				.collect::<Vec<_>>();
			tab.upload_files(
				Selector::parse(required(params.selector.as_deref(), "selector")?)
					.map_err(webview_fault)?,
				&paths,
				timeout,
			)
			.map_err(webview_fault)?;
			Value::Bool(true)
		},
		RunOperation::WaitForSelector => {
			tab.document()
				.wait_for_selector(
					Selector::parse(required(params.selector.as_deref(), "selector")?)
						.map_err(webview_fault)?,
					timeout,
				)
				.map_err(webview_fault)?;
			Value::Bool(true)
		},
		RunOperation::WaitForUrl => Value::String(
			tab.wait_for_url(required(params.value.as_deref(), "value")?, timeout)
				.map_err(webview_fault)?
				.to_string(),
		),
		RunOperation::WaitForResponse => Value::String(
			tab.wait_for_response(required(params.value.as_deref(), "value")?, timeout)
				.map_err(webview_fault)?
				.to_string(),
		),
	};
	Ok(Payload {
		action: Action::Run,
		name,
		url: Some(view.url()),
		title: Some(view.title()),
		result: Some(result),
		artifacts,
	})
}

fn element<'a>(
	view: &'a WebView,
	params: &Params,
) -> Result<omp_webview::automation::ElementHandle<'a>, Fault> {
	let selector =
		Selector::parse(required(params.selector.as_deref(), "selector")?).map_err(webview_fault)?;
	view
		.automation()
		.document()
		.resolve(selector)
		.map_err(webview_fault)
}

fn selector(value: Option<&str>) -> Result<Option<Selector>, Fault> {
	value
		.map(Selector::parse)
		.transpose()
		.map_err(webview_fault)
}

fn timeout(params: &Params) -> Duration {
	Duration::from_secs(params.timeout.unwrap_or(DEFAULT_TIMEOUT.as_secs())).min(MAX_TIMEOUT)
}

fn required<'a>(value: Option<&'a str>, field: &'static str) -> Result<&'a str, Fault> {
	value.ok_or_else(|| invalid(field))
}

fn invalid(message: &'static str) -> Fault {
	Fault { code: sf!("invalid_browser_request"), message: Str::new_static(message) }
}

fn not_found(name: &str) -> Fault {
	Fault { code: sf!("browser_tab_not_found"), message: sf!("browser tab `{name}` is not open") }
}

fn daemon_closed() -> Fault {
	Fault { code: sf!("browser_daemon_closed"), message: sf!("browser daemon is not available") }
}

fn webview_fault(error: omp_webview::Error) -> Fault {
	Fault { code: sf!("browser_automation_failed"), message: Str::new(error.to_string()) }
}

fn blob_fault(error: BlobError) -> Fault {
	Fault { code: sf!("browser_artifact_failed"), message: Str::new(error.to_string()) }
}
