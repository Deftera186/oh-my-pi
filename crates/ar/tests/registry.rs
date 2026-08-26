//! Shared archive-extension routing contracts.

use omp_ar::{EXTENSION_TABLE, Format, path_candidates};

#[test]
fn selector_candidates_cover_every_registered_archive_family() {
	let cases = [
		("pkg.deb:control.tar.xz", "pkg.deb", "control.tar.xz", Format::Deb),
		("bundle.7z:docs/readme.txt", "bundle.7z", "docs/readme.txt", Format::SevenZip),
		("image.iso:BOOT/catalog", "image.iso", "BOOT/catalog", Format::Iso),
		("source.tar.zst:src/lib.rs", "source.tar.zst", "src/lib.rs", Format::TarZst),
		("package.whl:module.py", "package.whl", "module.py", Format::Zip),
		("payload.xz:payload", "payload.xz", "payload", Format::Xz),
	];
	for (authored, archive_path, member_path, format) in cases {
		let candidate = path_candidates(authored).into_iter().next().unwrap();
		assert_eq!(candidate.archive_path, archive_path, "{authored}");
		assert_eq!(candidate.member_path, member_path, "{authored}");
		assert_eq!(candidate.format, format, "{authored}");
	}
}

#[test]
fn every_registry_extension_is_selector_routable() {
	for &(extension, format) in EXTENSION_TABLE {
		let authored = format!("fixture{extension}:member.txt");
		let candidate = path_candidates(&authored).into_iter().next().unwrap();
		assert_eq!(candidate.archive_path, format!("fixture{extension}"), "{extension}");
		assert_eq!(candidate.member_path, "member.txt", "{extension}");
		assert_eq!(candidate.format, format, "{extension}");
	}
}

#[test]
fn nested_archives_return_longest_container_candidate_first() {
	let candidates = path_candidates("outer.deb:data.tar.xz:usr/bin/tool");
	assert_eq!(candidates[0].archive_path, "outer.deb:data.tar.xz");
	assert_eq!(candidates[0].member_path, "usr/bin/tool");
	assert_eq!(candidates[1].archive_path, "outer.deb");
	assert_eq!(candidates[1].member_path, "data.tar.xz:usr/bin/tool");
}
