use super::{CardFixture, FixtureState};

const ARGS: &str = r#"{"questions":[
	{"id":"db","question":"Which database should the new service use?","options":[
		{"label":"Postgres","description":"Relational, strong consistency, JSONB support"},
		{"label":"SQLite","description":"Embedded, zero-ops, great for single-node"},
		{"label":"MongoDB","description":"Document store, flexible schema"}
	]},
	{"id":"features","question":"Which auth flows should ship in v1?","multi":true,"options":[
		{"label":"Email + password"},
		{"label":"OAuth (Google, GitHub)"},
		{"label":"Magic links"},
		{"label":"SAML SSO","description":"Enterprise; can be deferred"}
	]}
]}"#;
const RESULT: &str = r#"{"answers":[
	{"question":"db","options":["Postgres"]},
	{"question":"features","options":["Email + password","OAuth (Google, GitHub)"]}
]}"#;

pub(super) const FIXTURES: &[CardFixture] = &[CardFixture {
	tool:   "ask",
	title:  "Ask",
	states: [
		FixtureState {
			args:   r#"{"questions":[{"id":"db","question":"Which database should the new service use?","options":[{"label":"Postgres"#,
			update: None,
			result: None,
			fault:  None,
		},
		FixtureState { args: ARGS, update: None, result: None, fault: None },
		FixtureState { args: ARGS, update: None, result: Some(RESULT), fault: None },
		FixtureState {
			args:   ARGS,
			update: None,
			result: None,
			fault:  Some(r#""Prompt cancelled by user before any answer was given""#),
		},
	],
}];
