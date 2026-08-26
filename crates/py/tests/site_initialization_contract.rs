//! Authorized embedded-site initialization contract.

use std::{
	env, fs,
	path::PathBuf,
	process,
	time::{SystemTime, UNIX_EPOCH},
};

use omp_py::{Engine, pyo3::ffi::c_str};

fn temporary_root() -> PathBuf {
	let nonce = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.expect("clock after epoch")
		.as_nanos();
	env::temp_dir().join(format!("omp-py-site-{}-{nonce}", process::id()))
}

#[test]
fn authorized_site_processes_pth_and_sitecustomize() {
	let root = temporary_root();
	let site = root.join("site-packages");
	let editable = root.join("editable-src");
	fs::create_dir_all(&site).expect("create site directory");
	fs::create_dir_all(&editable).expect("create editable source");
	fs::write(editable.join("editable_package.py"), "VALUE = 'loaded-through-pth'\n")
		.expect("write editable package");
	fs::write(
		site.join("editable-install.pth"),
		format!("{}\nimport builtins; builtins.OMP_PTH_EXECUTED = True\n", editable.display()),
	)
	.expect("write pth metadata");
	fs::write(
		site.join("sitecustomize.py"),
		"import builtins\nbuiltins.OMP_SITECUSTOMIZE_EXECUTED = True\n",
	)
	.expect("write site customization");
	fs::write(
		site.join("usercustomize.py"),
		"import builtins\nbuiltins.OMP_USERCUSTOMIZE_EXECUTED = True\n",
	)
	.expect("write excluded user customization");

	let engine = Engine::builder()
		.site_packages(&site)
		.init()
		.expect("embedded Python boots");
	engine
		.attach(|py| {
			py.run(
				c_str!(
					r#"
import builtins
import editable_package

assert editable_package.VALUE == "loaded-through-pth"
assert builtins.OMP_PTH_EXECUTED is True
assert builtins.OMP_SITECUSTOMIZE_EXECUTED is True
assert not hasattr(builtins, "OMP_USERCUSTOMIZE_EXECUTED")
"#
				),
				None,
				None,
			)
		})
		.expect("authorized site behavior");

	fs::remove_dir_all(root).expect("remove test site");
}
