//! Clean-tree commit metadata, avatar, and identicon presentation.

use jiff::Timestamp;
use md5::{Digest as _, Md5};
use omp_core::{IntoStr, Str, sf};
use omp_tui::{
	Color, Prop, UiContext,
	components::{Col, Img, Tree},
	dom,
};

use super::{GitCommitInfo, short_sha};

pub(super) fn component(
	head: Option<&GitCommitInfo>,
	avatar: Option<bytes::Bytes>,
	ctx: &UiContext,
	file_tree: Tree,
	tree: bool,
	width: u16,
) -> Col {
	let Some(head) = head else {
		return dom! { <col w={width} justify=center align=center><text dim>{"No commits yet"}</text></col> };
	};
	let body = head.body.lines().take(8).map(Str::new).collect::<Vec<_>>();
	let author = head.author_name.clone();
	let email = head.author_email.clone();
	let authored_ms = authored_age_ms(head.author_date.as_str());
	let parents = head
		.parents
		.iter()
		.map(short_sha)
		.fold(String::new(), |mut output, parent| {
			if !output.is_empty() {
				output.push(' ');
			}
			output.push_str(parent.as_str());
			output
		});
	let additions = head
		.files
		.iter()
		.map(|file| file.additions.unwrap_or(0))
		.sum::<u64>();
	let deletions = head
		.files
		.iter()
		.map(|file| file.deletions.unwrap_or(0))
		.sum::<u64>();
	let file_count = head.files.len();
	let sha = short_sha(&head.sha);
	let identicon = identicon_lines(email.as_str(), ctx);
	let identicon_color = identicon_color(email.as_str());
	let image = avatar.map(|bytes| {
		Img::from_bytes(bytes)
			.with(Prop::W, 10_u16)
			.with(Prop::H, 3_u16)
	});
	let view = if tree { "tree" } else { "path" };
	dom! {
		<col w={width}>
			<text bold wrap>{head.subject.clone()}</text>
			if !body.is_empty() {
				<spacer h=1/>
				for line in body { <text fg=muted wrap>{line}</text> }
			}
			<spacer h=1/>
			if let Some(image) = image {
				{image}
			} else {
				for line in identicon { <pre fg={identicon_color}>{line}</pre> }
			}
			<row w={width} gap=1><text bold truncate>{author}</text><text dim truncate>{sf!("<{email}>")}</text></row>
			if let Some(age) = authored_ms {
				<row w={width} gap=1><text dim>{"authored"}</text><time kind="relative" ms={age} dim/></row>
			} else {
				<text dim truncate>{sf!("authored {}", head.author_date)}</text>
			}
			if !parents.is_empty() {
				<row w={width} gap=1><text dim>{"parent:"}</text><text fg=accent truncate>{parents}</text></row>
			}
			<hr fg=border/>
			<row w={width} gap=1>
				<text bold truncate grow>{sf!("{file_count} modified")}</text>
				<text fg=ok>{sf!("+{additions}")}</text>
				<text fg=err>{sf!("−{deletions}")}</text>
				<text dim>{sf!("· {sha}")}</text>
			</row>
			<row h=1 justify=center>
				<segmented id={super::sidebar::VIEW_STYLE_ID} value={view}>
					<option value="path" icon="view-path" label="Path"/>
					<option value="tree" icon="view-tree" label="Tree"/>
				</segmented>
			</row>
			{file_tree}
		</col>
	}
}

/// Milliseconds elapsed since the authored timestamp, clamped to zero for
/// future (clock-skewed) dates; `None` when the date string does not parse.
fn authored_age_ms(value: &str) -> Option<u64> {
	let then = value.parse::<Timestamp>().ok()?;
	Some(u64::try_from(Timestamp::now().duration_since(then).as_millis()).unwrap_or(0))
}

/// Builds deterministic mirrored 5×5 identicon rows for an email address.
pub(super) fn identicon_lines(email: &str, ctx: &UiContext) -> [Str; 3] {
	let digest: [u8; 16] = Md5::digest(email.trim().to_ascii_lowercase().as_bytes()).into();
	let upper = ctx.charset.icon(omp_tui::Icon::UpperHalf);
	let lower = ctx.charset.icon(omp_tui::Icon::LowerHalf);
	let full = ctx.charset.icon(omp_tui::Icon::Block);
	let on = |column: usize, row: usize| {
		let mirrored = if column < 3 { column } else { 4 - column };
		digest
			.get(3 + mirrored * 5 + row)
			.is_some_and(|byte| byte % 2 == 0)
	};
	std::array::from_fn(|pair| {
		let top = pair * 2;
		let bottom = top + 1;
		let mut line = String::with_capacity(10 * upper.len());
		for column in 0..5 {
			let cell = match (on(column, top), bottom < 5 && on(column, bottom)) {
				(true, true) => full,
				(true, false) => upper,
				(false, true) => lower,
				(false, false) => " ",
			};
			line.push_str(cell);
			line.push_str(cell);
		}
		line.into_str()
	})
}

fn identicon_color(email: &str) -> Color {
	let digest: [u8; 16] = Md5::digest(email.trim().to_ascii_lowercase().as_bytes()).into();
	let hue = u16::from_be_bytes([digest[0], digest[1]]) % 360;
	hsl_to_rgb(f32::from(hue), 0.55, 0.58)
}

fn hsl_to_rgb(hue: f32, saturation: f32, lightness: f32) -> Color {
	let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
	let sector = hue / 60.0;
	let x = chroma * (1.0 - (sector.rem_euclid(2.0) - 1.0).abs());
	let (red, green, blue) = match sector as u8 {
		0 => (chroma, x, 0.0),
		1 => (x, chroma, 0.0),
		2 => (0.0, chroma, x),
		3 => (0.0, x, chroma),
		4 => (x, 0.0, chroma),
		_ => (chroma, 0.0, x),
	};
	let match_value = lightness - chroma / 2.0;
	let channel = |value: f32| ((value + match_value) * 255.0).round() as u8;
	Color::Rgb(channel(red), channel(green), channel(blue))
}
