//! Replica-backed standalone collaboration guest composition.

use std::{env, io, io::IsTerminal as _, sync::Arc, time::Duration};

use miette::IntoDiagnostic as _;
use omp_collab::{guest::GuestRelayPump, link::CollabLink, presence::CollabRole};
use omp_core::Str;
use omp_driver::collab::session::{
	CollabOwnerCommand, CollabSessionAuthority, spawn_session_owner,
};
use omp_tool::Registry;

use crate::{chat_ui, cli::JoinArgs};

const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

/// Parses, joins, projects, and presents one remote collaboration until quit.
pub async fn run(args: JoinArgs) -> miette::Result<()> {
	if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
		return Err(miette::miette!("omp join requires an interactive terminal"));
	}
	let link_text = args.link.trim();
	let link = CollabLink::parse(link_text.as_str()).into_diagnostic()?;
	let data_dir = omp_core::dirs::data_dir(None).into_diagnostic()?;
	let local_cwd = env::current_dir().into_diagnostic()?;
	let settings = omp_driver::settings::current(&data_dir).into_diagnostic()?;
	let display_name = settings.collab.resolved_display_name();

	let (pump, replica) = GuestRelayPump::new(data_dir.join("collab"), local_cwd, chat_ui::now_ms());
	let replica_shutdown = replica.clone();
	let mut pump_task = tokio::spawn(pump.run());
	let (authority, collab) = CollabSessionAuthority::with_guest_replica(Some(replica.clone()));
	let mut owner_task = spawn_session_owner(authority);

	let joined = collab
		.request(CollabOwnerCommand::Join { link, display_name })
		.await
		.into_diagnostic();
	let outcome = match joined {
		Ok(_) => {
			let mut updates = replica.subscribe();
			tokio::time::timeout(SNAPSHOT_TIMEOUT, async {
				while !updates.borrow().ready {
					updates.changed().await.into_diagnostic()?;
				}
				Ok::<(), miette::Report>(())
			})
			.await
			.map_err(|_| miette::miette!("timed out waiting for the collaboration snapshot"))??;
			chat_ui::run_guest(replica, collab.clone(), Arc::new(Registry::new()), Str::default())
				.await
				.map(|_| ())
		},
		Err(error) => Err(error),
	};

	if collab
		.presence()
		.is_some_and(|facts| facts.role() == CollabRole::Guest)
	{
		let _ = collab.request(CollabOwnerCommand::Leave).await;
	}
	drop(collab);
	replica_shutdown.stop().await;
	if tokio::time::timeout(SHUTDOWN_TIMEOUT, &mut owner_task)
		.await
		.is_err()
	{
		owner_task.abort();
		let _ = owner_task.await;
	}
	if tokio::time::timeout(SHUTDOWN_TIMEOUT, &mut pump_task)
		.await
		.is_err()
	{
		pump_task.abort();
	}
	outcome
}
