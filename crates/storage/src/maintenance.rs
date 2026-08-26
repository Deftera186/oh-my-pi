//! Transactional maintenance operations over the derived sessions index.

use std::time::Duration;

use rusqlite::{Connection, OptionalExtension as _, Transaction, TransactionBehavior, params};

use crate::{
	index::{Error, SessionIndex},
	transcript::SessionId,
};

/// Whether maintenance changes are committed or measured and rolled back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaintenanceMode {
	/// Execute the complete transaction, then roll it back.
	DryRun,
	/// Commit the transaction.
	Apply,
}

/// Rows moved to a retained lineage representative and rows discarded because
/// that representative already owned the same logical key.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TransferCount {
	/// Rows rekeyed to the retained session.
	pub transferred: u64,
	/// Source rows discarded in favor of an existing retained row.
	pub collisions:  u64,
}

impl TransferCount {
	/// Total source rows examined by the transfer.
	#[must_use]
	pub const fn examined(self) -> u64 {
		self.transferred.saturating_add(self.collisions)
	}
}

/// Exact result of one lineage rekey transaction.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LineageTransferReport {
	/// Canonical inference receipt rows.
	pub receipts:          TransferCount,
	/// Message and tool outcome rows.
	pub item_outcomes:     TransferCount,
	/// Serving-model performance rows.
	pub model_performance: TransferCount,
	/// Distinct durable event-kind rows.
	pub entry_kinds:       TransferCount,
	/// Owner-private prompt-search rows.
	pub prompts_fts:       TransferCount,
	/// Archived session rows removed, or that would be removed in dry-run mode.
	pub archived_sessions: u64,
}

impl LineageTransferReport {
	/// Total rows rekeyed across every derived row family.
	#[must_use]
	pub const fn transferred(self) -> u64 {
		self
			.receipts
			.transferred
			.saturating_add(self.item_outcomes.transferred)
			.saturating_add(self.model_performance.transferred)
			.saturating_add(self.entry_kinds.transferred)
			.saturating_add(self.prompts_fts.transferred)
	}

	/// Total source rows discarded because the retained session already owned
	/// the same event-index or kind key.
	#[must_use]
	pub const fn collisions(self) -> u64 {
		self
			.receipts
			.collisions
			.saturating_add(self.item_outcomes.collisions)
			.saturating_add(self.model_performance.collisions)
			.saturating_add(self.entry_kinds.collisions)
			.saturating_add(self.prompts_fts.collisions)
	}
}

impl SessionIndex {
	/// Relocates one complete session projection to another authoritative index,
	/// updating its workspace identity while preserving all other session and
	/// derived accounting fields.
	///
	/// Source removal and destination insertion share one SQLite immediate
	/// transaction through an attached destination database. The destination
	/// must not already contain `session`. When both handles name the same
	/// database, this reduces to one transactional `cwd`/`project` update.
	/// Returns `false` without mutation when the source projection is absent.
	///
	/// This moves only the rebuildable index projection. The caller remains
	/// responsible for fencing the writer and relocating the journal through
	/// the active journal authority before reporting success.
	pub fn relocate_session(
		&self,
		destination: &Self,
		session: &SessionId,
		cwd: &str,
		project: &str,
	) -> Result<bool, Error> {
		self.require_writer()?;
		destination.require_writer()?;
		if std::ptr::eq(self, destination) {
			let mut connection = self.connection.lock();
			return update_session_location(&mut connection, session, cwd, project);
		}

		let source_address = std::ptr::from_ref(self).addr();
		let destination_address = std::ptr::from_ref(destination).addr();
		let (mut source, destination_connection) = if source_address < destination_address {
			(self.connection.lock(), destination.connection.lock())
		} else {
			let destination_connection = destination.connection.lock();
			let source = self.connection.lock();
			(source, destination_connection)
		};
		let source_path = main_database_path(&source)?;
		let destination_path = main_database_path(&destination_connection)?;
		if source_path.is_empty() || destination_path.is_empty() {
			return Err(Error::RelocationRequiresFileBackedIndexes);
		}
		if source_path == destination_path {
			return update_session_location(&mut source, session, cwd, project);
		}

		let mut relocation = Connection::open(&source_path)?;
		relocation.busy_timeout(Duration::from_secs(5))?;
		relocation.pragma_update(None, "foreign_keys", "ON")?;
		relocation
			.execute("ATTACH DATABASE ?1 AS relocation_destination", [destination_path.as_str()])?;
		relocate_attached(&mut relocation, session, cwd, project)
	}

	/// Deletes one session projection and all rows derived from it in a single
	/// immediate SQLite transaction.
	///
	/// The transcript journal remains durable truth and is intentionally not
	/// touched by this index API. Callers must stop the writer and remove the
	/// journal through the active session authority before reporting a complete
	/// user-facing deletion. Returns `false` when the projection was already
	/// absent.
	pub fn delete_session(&self, session: &SessionId) -> Result<bool, Error> {
		self.require_writer()?;
		let mut connection = self.connection.lock();
		let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
		transaction.execute("DELETE FROM prompts_fts WHERE session_id = ?1", [session.as_str()])?;
		let removed =
			transaction.execute("DELETE FROM sessions WHERE id = ?1", [session.as_str()])? != 0;
		transaction.commit()?;
		Ok(removed)
	}

	/// Rekeys every derived row owned by `archived` lineage members to
	/// `retained`, then removes the archived session rows in the same immediate
	/// SQLite transaction.
	///
	/// Receipt, outcome, and model rows use `(session_id, event_index)` as their
	/// logical key; entry kinds use `(session_id, kind)`. FTS rows follow the
	/// same event-index winner rule as their base event. On collision, the row
	/// already owned by the retained session wins, matching pi's
	/// `UPDATE OR IGNORE` maintenance semantics. Archived members are processed
	/// in caller order, so the first member wins collisions not already owned by
	/// the retained session.
	pub fn rekey_archived_lineage(
		&self,
		retained: &SessionId,
		archived: &[SessionId],
		mode: MaintenanceMode,
	) -> Result<LineageTransferReport, Error> {
		self.require_writer()?;
		if archived.iter().any(|session| session == retained) {
			return Err(Error::RetainedSessionArchived { session: retained.clone() });
		}
		let mut connection = self.connection.lock();
		let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
		require_session(&transaction, retained)?;
		for (ordinal, session) in archived.iter().enumerate() {
			if archived[..ordinal].contains(session) {
				continue;
			}
			require_session(&transaction, session)?;
		}
		transaction.execute_batch(
			"CREATE TEMP TABLE gc_retained_event_indexes (
			   event_index INTEGER PRIMARY KEY
			 ) WITHOUT ROWID;",
		)?;

		let mut report = LineageTransferReport::default();
		for (ordinal, source) in archived.iter().enumerate() {
			if archived[..ordinal].contains(source) {
				continue;
			}
			transaction.execute("DELETE FROM gc_retained_event_indexes", [])?;
			transaction.execute(
				"INSERT OR IGNORE INTO gc_retained_event_indexes(event_index)
				 SELECT event_index FROM receipts WHERE session_id = ?1
				 UNION SELECT event_index FROM item_outcomes WHERE session_id = ?1
				 UNION SELECT event_index FROM model_performance WHERE session_id = ?1
				 UNION SELECT event_index FROM prompts_fts WHERE session_id = ?1",
				[retained.as_str()],
			)?;
			transfer_event_table(&transaction, "receipts", retained, source, &mut report.receipts)?;
			transfer_event_table(
				&transaction,
				"item_outcomes",
				retained,
				source,
				&mut report.item_outcomes,
			)?;
			transfer_event_table(
				&transaction,
				"model_performance",
				retained,
				source,
				&mut report.model_performance,
			)?;
			transfer_keyed_table(
				&transaction,
				"session_entry_kinds",
				retained,
				source,
				&mut report.entry_kinds,
			)?;
			transfer_fts(&transaction, retained, source, &mut report.prompts_fts)?;
			report.archived_sessions = report.archived_sessions.saturating_add(
				u64::try_from(
					transaction.execute("DELETE FROM sessions WHERE id = ?1", [source.as_str()])?,
				)
				.expect("SQLite changed-row count fits u64"),
			);
		}
		transaction.execute_batch("DROP TABLE gc_retained_event_indexes;")?;
		transaction.execute(
			"UPDATE sessions SET turns = (
			   SELECT COUNT(*) FROM receipts WHERE session_id = ?1
			 ) WHERE id = ?1",
			[retained.as_str()],
		)?;
		finish(transaction, mode)?;
		Ok(report)
	}

	/// Removes archived sessions and every derived row without a retained
	/// lineage target. FTS rows are removed explicitly because the virtual table
	/// does not participate in foreign-key cascades.
	pub fn remove_archived_sessions(
		&self,
		archived: &[SessionId],
		mode: MaintenanceMode,
	) -> Result<u64, Error> {
		self.require_writer()?;
		let mut connection = self.connection.lock();
		let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
		for (ordinal, session) in archived.iter().enumerate() {
			if archived[..ordinal].contains(session) {
				continue;
			}
			require_session(&transaction, session)?;
		}
		let mut removed = 0_u64;
		for (ordinal, session) in archived.iter().enumerate() {
			if archived[..ordinal].contains(session) {
				continue;
			}
			transaction
				.execute("DELETE FROM prompts_fts WHERE session_id = ?1", [session.as_str()])?;
			removed = removed.saturating_add(
				u64::try_from(
					transaction.execute("DELETE FROM sessions WHERE id = ?1", [session.as_str()])?,
				)
				.expect("SQLite changed-row count fits u64"),
			);
		}
		finish(transaction, mode)?;
		Ok(removed)
	}
}

fn main_database_path(connection: &Connection) -> Result<String, Error> {
	Ok(connection.query_row(
		"SELECT file FROM pragma_database_list WHERE name = 'main'",
		[],
		|row| row.get(0),
	)?)
}

fn update_session_location(
	connection: &mut Connection,
	session: &SessionId,
	cwd: &str,
	project: &str,
) -> Result<bool, Error> {
	let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
	let updated =
		transaction.execute("UPDATE sessions SET cwd = ?2, project = ?3 WHERE id = ?1", params![
			session.as_str(),
			cwd,
			project
		])? != 0;
	transaction.commit()?;
	Ok(updated)
}

fn relocate_attached(
	source: &mut Connection,
	session: &SessionId,
	cwd: &str,
	project: &str,
) -> Result<bool, Error> {
	let transaction = source.transaction_with_behavior(TransactionBehavior::Immediate)?;
	let inserted = transaction.execute(
		"INSERT INTO relocation_destination.sessions (
		   id, title, title_source, cwd, project, created_ms, updated_ms, status, kind,
		   parent, parent_checkpoint, entries, turns, remote, journal_watermark,
		   last_event_index, repair_watermark, serving_provider, serving_model,
		   context_anchor, context_revision, compaction_epoch
		 )
		 SELECT id, title, title_source, ?2, ?3, created_ms, updated_ms, status, kind,
		        parent, parent_checkpoint, entries, turns, remote, journal_watermark,
		        last_event_index, repair_watermark, serving_provider, serving_model,
		        context_anchor, context_revision, compaction_epoch
		 FROM main.sessions WHERE id = ?1",
		params![session.as_str(), cwd, project],
	)? != 0;
	if !inserted {
		transaction.rollback()?;
		return Ok(false);
	}

	transaction.execute(
		"INSERT INTO relocation_destination.session_entry_kinds
		 SELECT * FROM main.session_entry_kinds WHERE session_id = ?1",
		[session.as_str()],
	)?;
	transaction.execute(
		"INSERT INTO relocation_destination.receipts
		 SELECT * FROM main.receipts WHERE session_id = ?1",
		[session.as_str()],
	)?;
	transaction.execute(
		"INSERT INTO relocation_destination.item_outcomes
		 SELECT * FROM main.item_outcomes WHERE session_id = ?1",
		[session.as_str()],
	)?;
	transaction.execute(
		"INSERT INTO relocation_destination.model_performance
		 SELECT * FROM main.model_performance WHERE session_id = ?1",
		[session.as_str()],
	)?;
	transaction.execute(
		"INSERT INTO relocation_destination.prompts_fts(session_id, event_index, prompt)
		 SELECT session_id, event_index, prompt FROM main.prompts_fts WHERE session_id = ?1",
		[session.as_str()],
	)?;
	transaction.execute("DELETE FROM main.prompts_fts WHERE session_id = ?1", [session.as_str()])?;
	transaction.execute("DELETE FROM main.sessions WHERE id = ?1", [session.as_str()])?;
	transaction.commit()?;
	Ok(true)
}

fn require_session(transaction: &Transaction<'_>, session: &SessionId) -> Result<(), Error> {
	let present = transaction
		.query_row("SELECT 1 FROM sessions WHERE id = ?1", [session.as_str()], |row| {
			row.get::<_, i64>(0)
		})
		.optional()?;
	if present.is_none() {
		return Err(Error::MissingMaintenanceSession { session: session.clone() });
	}
	Ok(())
}

fn transfer_keyed_table(
	transaction: &Transaction<'_>,
	table: &'static str,
	retained: &SessionId,
	source: &SessionId,
	report: &mut TransferCount,
) -> Result<(), Error> {
	let count_sql = format!("SELECT COUNT(*) FROM {table} WHERE session_id = ?1");
	let source_rows =
		transaction.query_row(&count_sql, [source.as_str()], |row| row.get::<_, u64>(0))?;
	let update_sql = format!("UPDATE OR IGNORE {table} SET session_id = ?1 WHERE session_id = ?2");
	let transferred =
		u64::try_from(transaction.execute(&update_sql, params![retained.as_str(), source.as_str()])?)
			.expect("SQLite changed-row count fits u64");
	let delete_sql = format!("DELETE FROM {table} WHERE session_id = ?1");
	transaction.execute(&delete_sql, [source.as_str()])?;
	report.transferred = report.transferred.saturating_add(transferred);
	report.collisions = report
		.collisions
		.saturating_add(source_rows.saturating_sub(transferred));
	Ok(())
}

fn transfer_event_table(
	transaction: &Transaction<'_>,
	table: &'static str,
	retained: &SessionId,
	source: &SessionId,
	report: &mut TransferCount,
) -> Result<(), Error> {
	let count_sql = format!("SELECT COUNT(*) FROM {table} WHERE session_id = ?1");
	let source_rows =
		transaction.query_row(&count_sql, [source.as_str()], |row| row.get::<_, u64>(0))?;
	let update_sql = format!(
		"UPDATE {table} SET session_id = ?1
		 WHERE session_id = ?2
		   AND event_index NOT IN (SELECT event_index FROM gc_retained_event_indexes)"
	);
	let transferred =
		u64::try_from(transaction.execute(&update_sql, params![retained.as_str(), source.as_str()])?)
			.expect("SQLite changed-row count fits u64");
	let delete_sql = format!("DELETE FROM {table} WHERE session_id = ?1");
	transaction.execute(&delete_sql, [source.as_str()])?;
	report.transferred = report.transferred.saturating_add(transferred);
	report.collisions = report
		.collisions
		.saturating_add(source_rows.saturating_sub(transferred));
	Ok(())
}

fn transfer_fts(
	transaction: &Transaction<'_>,
	retained: &SessionId,
	source: &SessionId,
	report: &mut TransferCount,
) -> Result<(), Error> {
	let source_rows = transaction.query_row(
		"SELECT COUNT(*) FROM prompts_fts WHERE session_id = ?1",
		[source.as_str()],
		|row| row.get::<_, u64>(0),
	)?;
	let transferred = u64::try_from(transaction.execute(
		"UPDATE prompts_fts SET session_id = ?1
		 WHERE session_id = ?2
		   AND event_index NOT IN (SELECT event_index FROM gc_retained_event_indexes)",
		params![retained.as_str(), source.as_str()],
	)?)
	.expect("SQLite changed-row count fits u64");
	transaction.execute("DELETE FROM prompts_fts WHERE session_id = ?1", [source.as_str()])?;
	report.transferred = report.transferred.saturating_add(transferred);
	report.collisions = report
		.collisions
		.saturating_add(source_rows.saturating_sub(transferred));
	Ok(())
}

fn finish(transaction: Transaction<'_>, mode: MaintenanceMode) -> Result<(), Error> {
	match mode {
		MaintenanceMode::DryRun => transaction.rollback()?,
		MaintenanceMode::Apply => transaction.commit()?,
	}
	Ok(())
}
