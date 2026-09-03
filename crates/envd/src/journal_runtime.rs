//! Closed legacy external-journal CONTROL boundary.
//!
//! Session reads now flow through the controller's `SessionAuthority` and DOM
//! patch subscription. The old transcript/index request vocabulary remains
//! rejected here while extension workers migrate to that typed boundary.

use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use omp_core::{Provenance, Str};
use serde_json::Value;

use super::{
	exthost::control::{
		ControlAuthority, ControlAuthorityFactory, ControlCompositionError,
		ControlConnectionIdentity, ControlEffect, ControlProtocolError, ControlRequestContext,
	},
	schedules::{ScheduleDeliveryBackend, open_durable_scheduler_unbound},
	server::EnvdError,
	worker::ExternalJournalCall,
};

/// Environment endpoint that rejects the retired raw-journal request surface.
#[derive(Clone)]
pub struct ExternalJournalActor {
	sender:    flume::Sender<ExternalJournalCall>,
	schedules: super::schedules::DurableScheduleHandle,
}

impl ExternalJournalActor {
	/// Starts the closed compatibility endpoint and durable scheduler owner.
	pub(crate) fn spawn(state_dir: &std::path::Path) -> Result<Self, EnvdError> {
		let schedules = open_durable_scheduler_unbound(&state_dir.join("agent-schedules.sqlite"))?;
		let (sender, receiver) = flume::unbounded::<ExternalJournalCall>();
		tokio::spawn(async move {
			while let Ok(call) = receiver.recv_async().await {
				let _ = call.reply.send(Err(Str::new_static(
					"raw external journal access was retired; use the session patch subscription",
				)));
			}
		});
		Ok(Self { sender, schedules })
	}

	/// Returns the endpoint installed into authenticated extension hosts.
	pub(crate) fn sender(&self) -> flume::Sender<ExternalJournalCall> {
		self.sender.clone()
	}

	/// Installs scheduled-delivery ownership.
	pub(crate) async fn bind_schedule_delivery(
		&self,
		backend: Arc<dyn ScheduleDeliveryBackend>,
	) -> Result<(), EnvdError> {
		Ok(self.schedules.bind_delivery(backend).await?)
	}
}

/// Closed persistence factory retained for the worker protocol shape.
pub struct PersistenceControlFactory;

impl PersistenceControlFactory {
	/// Constructs the closed persistence boundary.
	#[must_use]
	pub fn new(
		_actor: ExternalJournalActor,
		_provenances: Arc<BTreeMap<(Str, Str, Str), Provenance>>,
	) -> Self {
		Self
	}
}

impl ControlAuthorityFactory for PersistenceControlFactory {
	fn bind(
		&self,
		_identity: Arc<ControlConnectionIdentity>,
	) -> Result<Arc<dyn ControlAuthority>, ControlCompositionError> {
		Ok(Arc::new(ClosedPersistenceControl))
	}
}

struct ClosedPersistenceControl;

#[async_trait]
impl ControlAuthority for ClosedPersistenceControl {
	fn handles(&self, _operation: &str) -> bool {
		false
	}

	fn authorize(
		&self,
		_context: &ControlRequestContext,
		_operation: &str,
		_arguments: &serde_json::Map<String, Value>,
	) -> Result<(), ControlProtocolError> {
		Err(ControlProtocolError::new("unsupported", "raw journal CONTROL operations were retired"))
	}

	async fn request(
		&self,
		_context: ControlRequestContext,
		_operation: Str,
		_arguments: serde_json::Map<String, Value>,
	) -> Result<Value, ControlProtocolError> {
		Err(ControlProtocolError::new("unsupported", "raw journal CONTROL operations were retired"))
	}

	async fn effect(
		&self,
		_context: ControlRequestContext,
		_effect: ControlEffect,
	) -> Result<(), ControlProtocolError> {
		Err(ControlProtocolError::new("unsupported", "raw journal CONTROL operations were retired"))
	}
}
