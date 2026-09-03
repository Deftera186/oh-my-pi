use super::{CardFixture, FixtureState};

const STATES: [FixtureState; 4] = [
	FixtureState { args: "{}", update: None, result: None, fault: None },
	FixtureState { args: "{}", update: None, result: None, fault: None },
	FixtureState { args: "{}", update: None, result: Some("{}"), fault: None },
	FixtureState {
		args:   "{}",
		update: None,
		result: None,
		fault:  Some(r#""Tool execution failed""#),
	},
];

pub(super) const FIXTURES: &[CardFixture] =
	&[CardFixture { tool: "resolve", title: "", states: STATES }, CardFixture {
		tool:   "reject",
		title:  "",
		states: STATES,
	}];
