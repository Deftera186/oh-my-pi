//! Retained extension slot registry.
//!
//! Effects are applied synchronously at the host boundary. The registry owns
//! one [`Ui`] per mount, so composing a frame never reparses extension markup.

use std::{collections::HashMap, str};

use omp_core::{Str, fast_hash64};
use omp_proto::omp::ui::{
	v1,
	v1::{UiEffect, ui_effect},
};
use omp_tui::{Rect, Ui, UiContext};
use smallvec::SmallVec;
/// Maximum live mounts admitted from one extension registry.
pub const SLOT_MAX_PER_EXTENSION: usize = 32;

/// Sparse storage used for keyed extension mounts.
pub type SparseMap<K, V> = HashMap<K, V>;

/// Stable identity of an extension mount.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MountId(Str);

impl MountId {
	/// Creates an identity from the wire slot key.
	pub fn new(key: impl Into<Str>) -> Self {
		Self(key.into())
	}

	/// Returns the extension-provided key.
	pub fn as_str(&self) -> &str {
		self.0.as_str()
	}
}

/// One retained extension mount and its most recently resolved rectangle.
pub struct Mount {
	id:          MountId,
	placement:   i32,
	order:       i32,
	visible:     bool,
	width:       Option<u16>,
	height:      Option<u16>,
	source_hash: u64,
	ui:          Ui,
	rect:        Rect,
}

impl Mount {
	/// Returns the mount's stable identity.
	pub const fn id(&self) -> &MountId {
		&self.id
	}

	/// Returns the protocol placement discriminant.
	pub const fn placement(&self) -> i32 {
		self.placement
	}

	/// Returns the last rectangle resolved during composition.
	pub const fn rect(&self) -> Rect {
		self.rect
	}

	/// Returns the layout suggestion supplied by the extension.
	pub const fn order(&self) -> i32 {
		self.order
	}

	/// Returns whether this mount participates in layout.
	pub const fn visible(&self) -> bool {
		self.visible
	}

	pub(crate) const fn ui_mut(&mut self) -> &mut Ui {
		&mut self.ui
	}

	pub(crate) const fn preferred_width(&self) -> Option<u16> {
		self.width
	}

	pub(crate) const fn preferred_height(&self) -> Option<u16> {
		self.height
	}

	pub(crate) const fn resolve(&mut self, rect: Rect) {
		self.rect = rect;
	}
}
/// Headless description of a mount after its last band layout.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MountSnapshot {
	/// Extension-provided mount key.
	pub key:       Str,
	/// Protocol placement discriminant.
	pub placement: i32,
	/// Resolved frame rectangle.
	pub rect:      Rect,
}

/// Damage caused by a synchronous slot effect.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Damage {
	/// Rectangles whose extension surface needs repainting.
	pub rects:   SmallVec<Rect, 4>,
	/// Whether host-owned status chrome must be reconsidered.
	pub status:  bool,
	/// Explicit refusal, when an effect could not be admitted.
	pub refusal: Option<SlotRefusal>,
}

impl Damage {
	/// Returns whether applying the effect changed visible state.
	pub const fn is_empty(&self) -> bool {
		self.rects.is_empty() && !self.status && self.refusal.is_none()
	}

	fn mount(rect: Rect) -> Self {
		let mut rects = SmallVec::new();
		rects.push(rect);
		Self { rects, status: false, refusal: None }
	}

	const fn refused(refusal: SlotRefusal) -> Self {
		Self { rects: SmallVec::new(), status: false, refusal: Some(refusal) }
	}
}

/// Synchronous result of accepting or refusing an extension effect.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Apply {
	/// The effect was accepted and has exact repaint damage.
	Applied(Damage),
	/// A mount was refused rather than silently evicting another mount.
	Refused(SlotRefusal),
}

/// Explicit admission refusal for extension slot effects.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlotRefusal {
	/// The extension already owns [`SLOT_MAX_PER_EXTENSION`] live mounts.
	MountLimit,
	/// Extension markup could not be parsed into a retained UI.
	InvalidMarkup,
}

/// Retained mounts grouped by placement without allocating a frame-local list.
pub struct Slots {
	mounts:       SparseMap<MountId, Mount>,
	by_placement: SparseMap<i32, SmallVec<MountId, 4>>,
	ctx:          UiContext,
}

impl Slots {
	/// Creates an empty registry for one extension.
	pub fn new(ctx: UiContext) -> Self {
		Self { mounts: SparseMap::new(), by_placement: SparseMap::new(), ctx }
	}

	/// Synchronously applies one wire effect and reports exact typed damage.
	///
	/// Admission refusal is carried by [`Damage::refusal`], so callers never
	/// need to infer a silent eviction from registry state.
	pub fn apply(&mut self, effect: &UiEffect) -> Damage {
		match self.try_apply(effect) {
			Apply::Applied(damage) => damage,
			Apply::Refused(refusal) => Damage::refused(refusal),
		}
	}

	/// Synchronously applies one wire effect and reports typed damage.
	///
	/// A `Tml.hash` match skips parsing and repainting; the markup tree remains
	/// owned by its existing mount.
	pub fn try_apply(&mut self, effect: &UiEffect) -> Apply {
		match effect.kind.as_ref() {
			Some(ui_effect::Kind::MountSlot(mount)) => self.mount(mount),
			Some(ui_effect::Kind::UnmountSlot(unmount)) => {
				let id = MountId::new(unmount.key.clone());
				let Some(mount) = self.mounts.remove(&id) else {
					return Apply::Applied(Damage::default());
				};
				if let Some(ids) = self.by_placement.get_mut(&mount.placement) {
					ids.retain(|candidate| candidate != &id);
				}
				Apply::Applied(Damage::mount(mount.rect))
			},
			Some(ui_effect::Kind::SetStatus(_)) => {
				Apply::Applied(Damage { rects: SmallVec::new(), status: true, refusal: None })
			},
			_ => Apply::Applied(Damage::default()),
		}
	}

	/// Decodes and synchronously applies the serialized payload from the
	/// headless `effect` debug op.
	///
	/// # Errors
	/// Returns the JSON decode error before any retained mount is changed.
	pub fn apply_serialized(
		&mut self,
		payload: serde_json::Value,
	) -> Result<Damage, serde_json::Error> {
		let effect = serde_json::from_value::<UiEffect>(payload)?;
		Ok(self.apply(&effect))
	}

	/// Returns the mount count for this extension registry.
	pub fn len(&self) -> usize {
		self.mounts.len()
	}

	/// Returns whether no mount is live.
	pub fn is_empty(&self) -> bool {
		self.mounts.is_empty()
	}

	/// Returns mounts at a placement in their retained order.
	pub(crate) fn mounts_at_mut(&mut self, placement: i32) -> impl Iterator<Item = &mut Mount> {
		let ids = self
			.by_placement
			.get(&placement)
			.cloned()
			.unwrap_or_default();
		self
			.mounts
			.iter_mut()
			.filter(move |(id, _)| ids.contains(id))
			.map(|(_, mount)| mount)
	}

	/// Lists every retained mount for headless inspection.
	pub fn mounts(&self) -> impl Iterator<Item = &Mount> {
		self.mounts.values()
	}

	fn mount(&mut self, wire: &v1::MountSlot) -> Apply {
		let id = MountId::new(wire.key.clone());
		let content = wire.content.as_ref();

		let source = content.map_or("", |tml| str::from_utf8(&tml.source).unwrap_or(""));
		let hash = content.map_or(0, |tml| {
			if tml.hash == 0 {
				fast_hash64(tml.source.as_ref())
			} else {
				tml.hash
			}
		});
		if let Some(existing) = self.mounts.get(&id)
			&& existing.source_hash == hash
		{
			return Apply::Applied(Damage::default());
		}
		if !self.mounts.contains_key(&id) && self.mounts.len() >= SLOT_MAX_PER_EXTENSION {
			return Apply::Refused(SlotRefusal::MountLimit);
		}
		let options = wire.options.clone().unwrap_or_default();
		let width = options.width.and_then(|width| u16::try_from(width).ok());
		let ui = match Ui::from_extension_markup(
			Str::new(source),
			width.unwrap_or(1).max(1),
			self.ctx.clone(),
		) {
			Ok(ui) => ui,
			Err(_) => return Apply::Refused(SlotRefusal::InvalidMarkup),
		};
		let rect = self
			.mounts
			.get(&id)
			.map_or(Rect::new(0, 0, 0, 0), Mount::rect);
		let mount = Mount {
			id: id.clone(),
			placement: wire.placement,
			order: options.order,
			visible: options.visible,
			width,
			height: options.height.and_then(|height| u16::try_from(height).ok()),
			source_hash: hash,
			ui,
			rect,
		};
		let placement = mount.placement;
		let is_new = self.mounts.insert(id.clone(), mount).is_none();
		if is_new {
			self.by_placement.entry(placement).or_default().push(id);
		}
		Apply::Applied(Damage::mount(rect))
	}

	/// Produces mount keys and resolved rectangles for the `slots` debug op.
	pub fn debug_mounts(&self) -> SmallVec<MountSnapshot, 4> {
		self
			.mounts()
			.map(|mount| MountSnapshot {
				key:       mount.id.0.clone(),
				placement: mount.placement,
				rect:      mount.rect,
			})
			.collect()
	}
}

/// Lets user-owned layout take precedence over extension ordering suggestions.
pub fn arbitrate_order(user_order: Option<i32>, suggested_order: i32) -> i32 {
	user_order.unwrap_or(suggested_order)
}

/// User-owned final ordering table. Entries here override all extension
/// `SlotOptions.order` and `SetStatus(order=)` suggestions.
pub type UserLayout = SparseMap<MountId, i32>;

/// Resolves a suggested order against the user's saved layout.
pub fn arbitrate_with_user_layout(layout: &UserLayout, id: &MountId, suggested_order: i32) -> i32 {
	layout.get(id).copied().unwrap_or(suggested_order)
}

#[cfg(test)]
mod tests {
	use omp_proto::omp::ui::v1::{MountSlot, Tml, UiEffect, ui_effect};
	use omp_tui::UiContext;

	use super::{
		Apply, MountId, SLOT_MAX_PER_EXTENSION, SlotRefusal, Slots, UserLayout, arbitrate_order,
		arbitrate_with_user_layout,
	};

	fn effect(key: String, hash: u64) -> UiEffect {
		UiEffect {
			kind:  Some(ui_effect::Kind::MountSlot(MountSlot {
				key,
				placement: 1,
				content: Some(Tml { source: b"<text>x</text>".as_ref().into(), hash }),
				options: None,
			})),
			props: None,
		}
	}

	#[test]
	fn refuses_mount_over_cap_without_eviction() {
		let mut slots = Slots::new(UiContext::default());
		for number in 0..SLOT_MAX_PER_EXTENSION {
			assert!(matches!(
				slots.try_apply(&effect(number.to_string(), number as u64 + 1)),
				Apply::Applied(_)
			));
		}
		assert_eq!(
			slots.try_apply(&effect("too-many".to_owned(), 99)),
			Apply::Refused(SlotRefusal::MountLimit)
		);
		assert_eq!(slots.len(), SLOT_MAX_PER_EXTENSION);
	}

	#[test]
	fn tml_hash_short_circuits_parse_and_damage() {
		let mut slots = Slots::new(UiContext::default());
		let first = effect("same".to_owned(), 7);
		assert!(!slots.apply(&first).is_empty());
		assert!(slots.apply(&first).is_empty());
	}

	#[test]
	fn user_order_wins_arbitration() {
		let mut layout = UserLayout::new();
		let id = MountId::new("status");
		layout.insert(id.clone(), -4);
		assert_eq!(arbitrate_with_user_layout(&layout, &id, 20), -4);
		assert_eq!(arbitrate_with_user_layout(&layout, &MountId::new("other"), 20), 20);
		assert_eq!(arbitrate_order(Some(-4), 20), -4);
	}
}
