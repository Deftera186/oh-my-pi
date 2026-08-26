//! Frozen Python remote-shipping and public RPC SDK behavior contracts.

use omp_py::{Engine, pyo3::ffi::c_str};

#[test]
fn top_level_lambda_uses_pickle_and_rpc_sdk_is_importable() {
	let engine = Engine::builder().init().expect("embedded Python boots");
	engine
		.attach(|py| {
			py.run(
				c_str!(
					r#"
import importlib.util
import pathlib
import sys
import tempfile

import omp_remote
from omp_rpc import MessageUpdateEvent, RpcClient, parse_notification

with tempfile.TemporaryDirectory() as directory:
    module_path = pathlib.Path(directory) / "shipping_contract.py"
    module_path.write_text("named = lambda value: value + 1\n", encoding="utf-8")
    spec = importlib.util.spec_from_file_location("shipping_contract", module_path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    assert omp_remote._default_ship(module.named) == "pickle"
    assert omp_remote._pack_function(module.named, None)[1]

client = RpcClient(command=("omp", "--mode", "rpc"))
assert client is not None
notification = parse_notification({
    "type": "message_update",
    "message": {"role": "assistant"},
    "assistantMessageEvent": {
        "type": "text_delta",
        "contentIndex": 0,
        "partial": {"role": "assistant"},
        "delta": "hello",
    },
})
assert isinstance(notification, MessageUpdateEvent)
assert notification.assistant_message_event["delta"] == "hello"
"#
				),
				None,
				None,
			)
		})
		.expect("remote shipping and RPC SDK contract");
}
