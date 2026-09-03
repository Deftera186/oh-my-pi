//! Process-local owner for a replica-backed collaboration relay.

use omp_collab::{
	link::CollabLink,
	presence::{ConnectionState, PresenceFacts},
	relay::{RelayClient, RelayRole},
};
use omp_core::Str;
use tokio::{sync::watch, task::JoinHandle};

/// One operation serialized through the collaboration owner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CollabOwnerCommand {
	/// Join a parsed room link under the resolved local identity.
	Join {
		/// Parsed room endpoint and credentials.
		link:         CollabLink,
		/// Local participant name.
		display_name: Str,
	},
	/// Leave the active room.
	Leave,
}

/// Settled collaboration command result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CollabCommandResult {
	/// Current presence facts.
	pub presence: Option<PresenceFacts>,
}

/// Collaboration owner failure.
#[derive(Debug, thiserror::Error)]
pub enum CollabCommandFault {
	/// Owner task has stopped.
	#[error("collaboration owner stopped")]
	OwnerStopped,
	/// Relay operation failed.
	#[error("collaboration relay failed")]
	Relay(#[from] omp_collab::relay::RelayError),
	/// Room key was invalid.
	#[error("collaboration room key was invalid")]
	Crypto(#[from] omp_collab::crypto::CryptoError),
	/// No room is active.
	#[error("not joined to a collaboration room")]
	NotJoined,
}

struct Request {
	command: CollabOwnerCommand,
	reply:   flume::Sender<Result<CollabCommandResult, CollabCommandFault>>,
}

/// Cloneable command and presence projection.
#[derive(Clone)]
pub struct CollabCommandHandle {
	commands: flume::Sender<Request>,
	presence: watch::Receiver<Option<PresenceFacts>>,
}

impl CollabCommandHandle {
	/// Requests one serialized owner operation.
	pub async fn request(
		&self,
		command: CollabOwnerCommand,
	) -> Result<CollabCommandResult, CollabCommandFault> {
		let (reply, result) = flume::bounded(1);
		self
			.commands
			.send_async(Request { command, reply })
			.await
			.map_err(|_| CollabCommandFault::OwnerStopped)?;
		result
			.recv_async()
			.await
			.map_err(|_| CollabCommandFault::OwnerStopped)?
	}

	/// Returns current presence facts.
	#[must_use]
	pub fn presence(&self) -> Option<PresenceFacts> {
		*self.presence.borrow()
	}

	/// Subscribes to presence changes.
	#[must_use]
	pub fn subscribe_presence(&self) -> watch::Receiver<Option<PresenceFacts>> {
		self.presence.clone()
	}
}

/// Receiving half retained by the relay lifecycle owner.
pub struct CollabSessionAuthority {
	commands: flume::Receiver<Request>,
	presence: watch::Sender<Option<PresenceFacts>>,
}

impl CollabSessionAuthority {
	/// Constructs the collaboration owner.
	#[must_use]
	pub fn new() -> (Self, CollabCommandHandle) {
		let (commands, requests) = flume::bounded(16);
		let (presence, observed) = watch::channel(None);
		(Self { commands: requests, presence }, CollabCommandHandle { commands, presence: observed })
	}

	async fn run(self) {
		let mut relay: Option<RelayClient> = None;
		while let Ok(request) = self.commands.recv_async().await {
			let result = match request.command {
				CollabOwnerCommand::Join { link, display_name: _ } => {
					if let Some(active) = relay.as_mut() {
						let _ = active.close().await;
					}
					let key = omp_collab::crypto::RoomKey::from_bytes(*link.credentials().key());
					match key {
						Ok(key) => match RelayClient::new(link.room_url(), RelayRole::Guest, key) {
							Ok(mut client) => match client.connect().await {
								Ok(()) => {
									let facts = PresenceFacts::guest(
										ConnectionState::Connected,
										1,
										link.credentials().is_read_only(),
									);
									self.presence.send_replace(Some(facts));
									relay = Some(client);
									Ok(CollabCommandResult { presence: Some(facts) })
								},
								Err(error) => Err(error.into()),
							},
							Err(error) => Err(error.into()),
						},
						Err(error) => Err(error.into()),
					}
				},
				CollabOwnerCommand::Leave => match relay.as_mut() {
					Some(active) => match active.close().await {
						Ok(()) => {
							relay = None;
							self.presence.send_replace(None);
							Ok(CollabCommandResult { presence: None })
						},
						Err(error) => Err(error.into()),
					},
					None => Err(CollabCommandFault::NotJoined),
				},
			};
			let _ = request.reply.send(result);
		}
	}
}

/// Starts the native relay-backed command owner.
#[must_use]
pub fn spawn_session_owner(authority: CollabSessionAuthority) -> JoinHandle<()> {
	tokio::spawn(authority.run())
}
