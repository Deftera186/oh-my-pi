use super::{CardFixture, FixtureState};

const ARGS: &str = r#"{"action":"stack_trace","levels":20}"#;
const RESULT: &str = r#"{"action":"stack_trace","session":{"id":"dbg-1","adapter":"debugpy","cwd":"/Users/dev/project","program":"./app/server.py","status":"stopped","reason":"breakpoint","frame":"validate_token","instruction_pointer":"0x00000001000034a8","path":"app/server.py","line":42,"col":14},"frames":[{"id":1000,"name":"validate_token","path":"app/server.py","line":42,"col":14},{"id":1001,"name":"authenticate","path":"app/server.py","line":88,"col":9},{"id":1002,"name":"handle_request","path":"app/router.py","line":153,"col":20},{"id":1003,"name":"dispatch","path":"app/router.py","line":97,"col":5},{"id":1004,"name":"<module>","path":"app/server.py","line":212,"col":1}]}"#;

pub(super) const FIXTURES: &[CardFixture] = &[CardFixture {
	tool:   "debug",
	title:  "Debug",
	states: [
		FixtureState {
			args:   r#"{"action":"stack_trace"#,
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
			fault:  Some(r#""No active debug session. Launch or attach first.""#),
		},
	],
}];
