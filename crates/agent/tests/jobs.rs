//! Rewind-to-runtime lifecycle integration for the shared job primitive.

use omp_agent::JobBoard;
use omp_core::Str;
use omp_session::{
	ComponentRegistry, Session,
	components::jobs::{self, JobSpec},
};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

#[test]
fn jobs_rewind_removing_a_subagent_terminates_it() {
	let temp = tempdir().expect("temporary session directory");
	let path = temp.path().join("parent.oms");
	let mut session = Session::create(&path, ComponentRegistry::standard()).expect("create session");
	let before = session.head().expect("genesis head");
	let txn = jobs::insert(session.dom(), before, JobSpec {
		id:      Str::new_static("child-1"),
		kind:    Str::new_static("subagent"),
		owner:   Str::new_static("Main"),
		started: Str::new_static("1"),
		agent:   Some(Str::new_static("task")),
	})
	.expect("jobs root");
	session.patch(txn).expect("insert subagent");

	let handle = session
		.dom()
		.select("jobs subagent[id=child-1]")
		.expect("valid selector")
		.into_iter()
		.next()
		.expect("subagent element");
	let cancel = CancellationToken::new();
	let board = JobBoard::new();
	assert!(board.attach(session.dom(), handle, cancel.clone()));

	let work = session.rewind(before).expect("rewind before spawn");
	assert_eq!(work.terminate, vec![handle]);
	board.apply_lifecycle(&work);
	assert!(cancel.is_cancelled());
	assert!(board.list().is_empty());
}
