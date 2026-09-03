//! Foreign transcript import contracts for native journals.

use std::fs;

use omp_app::session_import::{ForeignFormat, import_file};
use omp_session::{ComponentRegistry, Session};

#[test]
fn claude_fixture_imports_to_native_journal() {
	let directory = tempfile::tempdir().expect("tempdir");
	let source = directory.path().join("claude.jsonl");
	let destination = directory.path().join("claude.oms");
	fs::write(
		&source,
		r#"{"type":"user","message":{"role":"user","content":"hello"}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"world"}]}}
"#,
	)
	.expect("fixture");
	assert_eq!(import_file(ForeignFormat::Claude, &source, &destination).expect("import"), 2);
	let session = Session::open(&destination, ComponentRegistry::standard()).expect("open");
	assert_eq!(omp_app::print_mode::transcript_text(session.dom()), "world\n");
}

#[test]
fn codex_fixture_imports_to_native_journal() {
	let directory = tempfile::tempdir().expect("tempdir");
	let source = directory.path().join("codex.jsonl");
	let destination = directory.path().join("codex.oms");
	fs::write(
		&source,
		r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"ping"}]}}
{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"pong"}]}}
"#,
	)
	.expect("fixture");
	assert_eq!(import_file(ForeignFormat::Codex, &source, &destination).expect("import"), 2);
	let session = Session::open(&destination, ComponentRegistry::standard()).expect("open");
	assert_eq!(omp_app::print_mode::transcript_text(session.dom()), "pong\n");
}
