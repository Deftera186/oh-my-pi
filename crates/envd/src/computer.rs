//! Admission-routed native desktop session host.

use std::sync::Arc;

use async_trait::async_trait;
use omp_core::{ArtifactUrl, Str, sf};
use omp_desktop::{
	AxNode, AxQuery, AxSnapshotOptions, CaptureCaps, DesktopPoint, DesktopSession, Target,
};
use omp_tools::computer::{Action, ComputerHost, Fault, Params, Payload};
use serde_json::{Value, json};

use super::blobs::{BlobError, BlobHost};

/// Persistent native desktop owner shared by every `computer` invocation in a
/// session-scoped Environment registry.
pub(crate) struct ComputerSessionHost {
	session: DesktopSession,
	blobs:   BlobHost,
}

impl ComputerSessionHost {
	pub(crate) fn new(blobs: BlobHost) -> Arc<Self> {
		Arc::new(Self { session: DesktopSession::new(None), blobs })
	}
}

#[async_trait]
impl ComputerHost for ComputerSessionHost {
	async fn execute(&self, params: Params) -> Result<Payload, Fault> {
		let action = params.action;
		let mut artifacts = Vec::new();
		let result = match action {
			Action::Capabilities => {
				capabilities(self.session.capabilities().await.map_err(native_fault)?)
			},
			Action::ListDisplays => Value::Array(
				self
					.session
					.list_displays()
					.await
					.map_err(native_fault)?
					.into_iter()
					.map(|display| {
						json!({
							"id": display.id,
							"name": display.name,
							"x": display.x,
							"y": display.y,
							"width": display.width,
							"height": display.height,
							"scale": display.scale,
							"pixel_x": display.pixel_x,
							"pixel_y": display.pixel_y,
							"pixel_width": display.pixel_width,
							"pixel_height": display.pixel_height,
							"primary": display.is_primary,
						})
					})
					.collect(),
			),
			Action::ListWindows => Value::Array(
				self
					.session
					.list_windows()
					.await
					.map_err(native_fault)?
					.into_iter()
					.map(|window| {
						json!({
							"id": window.id,
							"title": window.title,
							"app": window.app,
							"pid": window.pid,
							"x": window.x,
							"y": window.y,
							"width": window.width,
							"height": window.height,
							"focused": window.focused,
						})
					})
					.collect(),
			),
			Action::Capture => {
				let capture = self
					.session
					.capture(target(&params), CaptureCaps {
						max_width:  params.max_width,
						max_height: params.max_height,
					})
					.await
					.map_err(native_fault)?;
				let id = self.blobs.put(&capture.data).map_err(blob_fault)?;
				let artifact = Str::new(ArtifactUrl::from_digest(id.hash).as_str());
				artifacts.push(artifact.clone());
				json!({
					"artifact": artifact,
					"bytes": id.size,
					"width": capture.width,
					"height": capture.height,
					"source_width": capture.source_width,
					"source_height": capture.source_height,
					"target": capture.target,
					"backend": capture.backend,
					"display_server": capture.display_server,
				})
			},
			Action::Click => {
				self
					.session
					.click(target(&params), number(params.x, "x")?, number(params.y, "y")?, None)
					.await
					.map_err(native_fault)?;
				Value::Bool(true)
			},
			Action::MoveMouse => {
				self
					.session
					.move_mouse(target(&params), number(params.x, "x")?, number(params.y, "y")?, None)
					.await
					.map_err(native_fault)?;
				Value::Bool(true)
			},
			Action::Drag => {
				let points = params
					.points
					.as_ref()
					.ok_or_else(|| invalid("drag requires `points`"))?
					.iter()
					.map(|point| DesktopPoint { x: point[0], y: point[1] })
					.collect();
				self
					.session
					.drag(target(&params), points, None)
					.await
					.map_err(native_fault)?;
				Value::Bool(true)
			},
			Action::Scroll => {
				self
					.session
					.scroll(
						target(&params),
						number(params.x, "x")?,
						number(params.y, "y")?,
						number(params.dx, "dx")?,
						number(params.dy, "dy")?,
						None,
					)
					.await
					.map_err(native_fault)?;
				Value::Bool(true)
			},
			Action::Type => {
				self
					.session
					.type_text(target(&params), text(&params)?.to_owned(), None)
					.await
					.map_err(native_fault)?;
				Value::Bool(true)
			},
			Action::KeyChord => {
				let keys = text(&params)?
					.split('+')
					.map(str::trim)
					.filter(|key| !key.is_empty())
					.map(str::to_owned)
					.collect::<Vec<_>>();
				self
					.session
					.key_chord(target(&params), &keys, None)
					.await
					.map_err(native_fault)?;
				Value::Bool(true)
			},
			Action::RaiseWindow => {
				self
					.session
					.raise_window(
						required(
							params.reference.as_deref().or(params.window.as_deref()),
							"raise_window requires `reference` or `window`",
						)?
						.to_owned(),
					)
					.await
					.map_err(native_fault)?;
				Value::Bool(true)
			},
			Action::AxSnapshot => {
				let snapshot = self
					.session
					.ax_snapshot(target(&params), AxSnapshotOptions {
						max_depth: params.max_depth,
						max_nodes: params.limit,
						all:       None,
					})
					.await
					.map_err(native_fault)?;
				json!({ "text": snapshot.text, "node_count": snapshot.node_count, "truncated": snapshot.truncated })
			},
			Action::AxQuery => Value::Array(
				self
					.session
					.ax_query(target(&params), AxQuery {
						role:  params.value.as_ref().map(ToString::to_string),
						title: None,
						value: None,
						limit: params.limit,
					})
					.await
					.map_err(native_fault)?
					.into_iter()
					.map(node)
					.collect(),
			),
			Action::AxElementAt => self
				.session
				.ax_element_at(target(&params), number(params.x, "x")?, number(params.y, "y")?)
				.await
				.map_err(native_fault)?
				.map(node)
				.unwrap_or(Value::Null),
			Action::AxFocused => self
				.session
				.ax_focused()
				.await
				.map_err(native_fault)?
				.map(node)
				.unwrap_or(Value::Null),
			Action::AxNode => node(
				self
					.session
					.ax_node(
						required(params.reference.as_deref(), "ax_node requires `reference`")?.to_owned(),
					)
					.await
					.map_err(native_fault)?,
			),
			Action::AxAttributes => Value::Array(
				self
					.session
					.ax_attributes(
						required(params.reference.as_deref(), "ax_attributes requires `reference`")?
							.to_owned(),
					)
					.await
					.map_err(native_fault)?
					.into_iter()
					.map(|(name, value)| json!({ "name": name, "value": value }))
					.collect(),
			),
		};
		Ok(Payload { action, result, artifacts })
	}
}

fn target(params: &Params) -> Target {
	params
		.window
		.as_deref()
		.map_or(Target::Desktop, Target::parse)
}

fn capabilities(value: omp_desktop::DesktopCapabilities) -> Value {
	json!({
		"backend": value.backend,
		"display_server": value.display_server,
		"capture": value.capture,
		"input": value.input,
		"ax": value.ax,
		"background_window_input": value.background_window_input,
		"delivery_modes": value.delivery_modes,
		"capture_permission": value.capture_permission,
		"input_permission": value.input_permission,
		"ax_permission": value.ax_permission,
		"display_count": value.display_count,
	})
}

fn node(value: AxNode) -> Value {
	json!({
		"ref": value.ref_,
		"role": value.role,
		"native_role": value.native_role,
		"title": value.title,
		"value": value.value,
		"description": value.description,
		"enabled": value.enabled,
		"focused": value.focused,
		"x": value.x,
		"y": value.y,
		"width": value.width,
		"height": value.height,
		"actions": value.actions,
		"child_count": value.child_count,
	})
}

fn text(params: &Params) -> Result<&str, Fault> {
	required(params.value.as_deref(), "operation requires `value`")
}

fn number(value: Option<f64>, field: &'static str) -> Result<f64, Fault> {
	value.ok_or_else(|| invalid(field))
}

fn required<'a>(value: Option<&'a str>, message: &'static str) -> Result<&'a str, Fault> {
	value.ok_or_else(|| invalid(message))
}

fn invalid(message: &'static str) -> Fault {
	Fault { code: sf!("invalid_desktop_request"), message: Str::new_static(message) }
}

fn native_fault(error: omp_desktop::DesktopError) -> Fault {
	Fault { code: sf!("desktop_operation_failed"), message: Str::new(error.to_string()) }
}

fn blob_fault(error: BlobError) -> Fault {
	Fault { code: sf!("desktop_artifact_failed"), message: Str::new(error.to_string()) }
}
