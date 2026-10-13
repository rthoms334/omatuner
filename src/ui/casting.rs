//! Casting from where things are chosen (DESIGN §40 §3.1 "Cast to…" on an item, §3.9 the Play
//! on switch): the browse screens' share of casting, hooked in like the batch features' item
//! menus and keys (`ui::item_menu::extend` / `dispatch`).
//!
//! **Cast to…** on a channel row, a Home tile, a card, a programme, an episode or a title's page
//! (its menu; `C` where typing doesn't filter — the guide, detail pages — and Alt+C everywhere an
//! item is focused, `Action::CastItem`): with exactly one TV ready ([`ready`]: usable and paired,
//! or needing no pairing) it casts straight there and toasts "Casting BBC One to Living Room";
//! otherwise the picker opens for that item (`OverlayRequest::CastItem`), which pairs a TV first
//! when it has to. The item plays as Shift+Enter plays it here — the same PIN prompt, the same
//! copy and its fallback, the same Resume / Start over prompt for a film or an episode — only
//! its commands go to the TV ([`UiCtx::cast_with`]: `Play` → `CastTo`, `PlayCopy` → `CastCopy`)
//! and the screen stays where it is (the mini player shows the casting card). Nothing plays
//! here first: the TV's first load is the relay's, under picture A's lease (§10). A programme
//! casts its channel while it airs and its catch-up once it aired; one that hasn't started
//! isn't offered (Remind me can switch to it). A locked item asks for the PIN with
//! `Action::CastUnlocked`, which comes back to the app ([`unlocked`]) and casts it then. Two
//! pictures can't cast (§0.6): the entry isn't offered in multi-view.
//!
//! **Play on** (§3.9): picture A's controller holds the switch (`PlayerCommand::SetPlayOn`,
//! `PlayerSnapshot::play_on`), so every play of the main picture follows it — the screens' Enter,
//! Up next, a reminder's switch, the agent's `play` without `device`, `--play` — except the Live
//! preview's (moving the cursor never casts). [`choose`] is the chooser's (the status chip,
//! Alt+T, Settings ▸ Cast): like a speaker picker, what plays here moves to the TV chosen (the
//! Live preview stops instead), and "This computer" while casting is Play here. Failures,
//! Stop casting and Play here leave the switch as it is: only the user changes it. It is
//! written to `cast.play_on` whenever it changes and read back at start only with "Remember
//! Play on across restarts" (`cast.remember_play_on`, off: a TV that is off at the next start
//! never swallows plays).

use std::cell::RefCell;

use crate::cast::{Device, Pairing};
use crate::keymap::{Action, KeyContext};
use crate::model::{DeviceId, MediaRef, PlayOrigin, TileId, VodKind};
use crate::msg::Toast;
use crate::player::{PlayRequest, PlayerCommand, PlayerSnapshot, PlayerState};
use crate::ui::item_menu::Target;
use crate::ui::overlays::MenuItem;
use crate::ui::widgets::glyphs;
use crate::ui::{OverlayRequest, UiCtx};

/// Where "Cast to…" sends this action's plays, and what its toast says.
#[derive(Debug, Clone, PartialEq)]
pub struct CastDest {
    pub device: DeviceId,
    /// The TV's (sanitized) name.
    pub tv: String,
    /// What is cast ("BBC One", "Dune", "Slow Horses S02E03"): never a programme's title, which
    /// the spoiler guard may hide.
    pub title: String,
}

/// The picker opened for one item (`OverlayRequest::CastItem`).
#[derive(Debug, Clone, PartialEq)]
pub struct ItemCast {
    pub target: Target,
    pub title: String,
}

/// What the PIN pairing overlay does once paired (`OverlayRequest::CastPairThen`).
#[derive(Debug, Clone, PartialEq)]
pub enum AfterPair {
    /// Settings ▸ Cast and the setup wizard: pairing only.
    Nothing,
    /// The player's picker and the casting card: what plays moves to the TV.
    CastPlaying,
    /// "Cast to…" on an item: that item, there.
    Item(ItemCast),
    /// The Play on chooser: the switch goes to the TV.
    PlayOn,
}

thread_local! {
    /// The last item "Cast to…" asked the PIN for, and where it goes: [`unlocked`] casts it
    /// (only the newest prompt answers, `Action::CastUnlocked` comes from nowhere else).
    static PENDING: RefCell<Option<(Target, CastDest)>> = const { RefCell::new(None) };
}

/// The TVs a cast starts on without a question (most recently used first, the registry's
/// order): usable, and paired or needing no pairing. Exactly one: `C` casts straight to it.
pub fn ready(devices: &[Device]) -> Vec<&Device> {
    devices.iter().filter(|d| d.selectable() && matches!(d.pairing, Pairing::Paired(_) | Pairing::NotNeeded)).collect()
}

/// Its entry in an item menu: "Cast to Living Room" with one TV ready, else "Cast to…" (the
/// picker); not for a programme that hasn't started, not while casting is off or in
/// multi-view.
pub fn menu_items(target: &Target, ctx: &UiCtx<'_>, keys: KeyContext) -> Vec<MenuItem> {
    if !crate::cast::on(ctx.config) || ctx.multiview.on() || castable(ctx, target).is_err() {
        return Vec::new();
    }
    let devices = ctx.services.agent.cast().devices();
    let label = match ready(&devices)[..] {
        [one] => format!("Cast to {}", one.name),
        _ => "Cast to…".to_owned(),
    };
    vec![crate::ui::item_menu::entry(glyphs::CAST, label, Action::CastItem, keys)]
}

/// `Action::CastItem` on `target` (a key or the menu): straight to the one TV ready, else the
/// picker for it.
pub fn on_action(ctx: &mut UiCtx<'_>, action: Action, target: &Target) -> bool {
    if action != Action::CastItem || !crate::cast::on(ctx.config) {
        return false;
    }
    if ctx.multiview.on() {
        ctx.toast(Toast::info("Leave multi-view to cast"));
        return true;
    }
    let title = match castable(ctx, target) {
        Ok(t) => t,
        Err(why) => {
            ctx.toast(Toast::info(why));
            return true;
        }
    };
    let devices = ctx.services.agent.cast().devices();
    match ready(&devices)[..] {
        [one] => {
            let dest = CastDest { device: one.id, tv: one.name.clone(), title };
            cast_target(ctx, dest, target);
        }
        _ => ctx.open(OverlayRequest::CastItem(ItemCast { target: *target, title })),
    }
    true
}

/// Cast `target` to `dest`'s TV, as Shift+Enter would play it here (module docs).
pub fn cast_target(ctx: &mut UiCtx<'_>, dest: CastDest, target: &Target) {
    if ctx.multiview.on() {
        ctx.toast(Toast::info("Leave multi-view to cast"));
        return;
    }
    PENDING.with_borrow_mut(|p| *p = Some((*target, dest.clone())));
    ctx.cast_with(dest, |ctx| play(ctx, target));
}

/// The PIN prompt "Cast to…" opened was answered (`Action::CastUnlocked`, routed by the app):
/// cast the item now.
pub fn unlocked(ctx: &mut UiCtx<'_>) {
    if let Some((target, dest)) = PENDING.with_borrow_mut(Option::take) {
        cast_target(ctx, dest, &target);
    }
}

/// What "Cast to…" calls `target`, or why it can't be cast now.
fn castable(ctx: &UiCtx<'_>, target: &Target) -> Result<String, &'static str> {
    const GONE: &str = "That is no longer in your playlists";
    match *target {
        Target::Channel(id) => ctx.lib.live.get(id).map(|c| c.name.to_string()).ok_or(GONE),
        Target::Movie(_) | Target::Series(_) => {
            let kind = match *target {
                Target::Movie(id) => VodKind::Movie(id),
                Target::Series(id) => VodKind::Series(id),
                _ => unreachable!(),
            };
            ctx.lib.vod.find(kind).map(|v| v.name.to_string()).ok_or(GONE)
        }
        Target::Episode { series, episode } => {
            let name = ctx.lib.vod.find(VodKind::Series(series)).map(|v| v.name.to_string()).ok_or(GONE)?;
            let ep = crate::store::vod::episode(ctx.db, episode).map_err(|_| GONE)?;
            Ok(format!("{name} S{:02}E{:02}", ep.season, ep.episode))
        }
        Target::Programme { programme, channel } => match airing(ctx, programme, channel) {
            Some((Airing::Live, ch)) => Ok(ch),
            Some((Airing::Past { .. }, ch)) => Ok(format!("{ch} (catch-up)")),
            Some((Airing::NotYet, _)) => Err("It hasn't started yet — Remind me (r) can switch to it"),
            Some((Airing::NoArchive, _)) => Err("This programme isn't in the channel's archive"),
            None => Err(GONE),
        },
    }
}

/// Where a guide programme stands now.
enum Airing {
    Live,
    /// Aired, on a channel with catch-up: (start, stop) as the channel shows it, its id.
    Past {
        start: crate::model::Timestamp,
        stop: crate::model::Timestamp,
        catchup_id: Option<String>,
    },
    NotYet,
    NoArchive,
}

/// The programme's state on `channel` (its tvg-shift applied) and the channel's name; `None`
/// when either is gone. One indexed read.
fn airing(ctx: &UiCtx<'_>, programme: crate::model::ProgrammeId, channel: crate::model::ChannelId) -> Option<(Airing, String)> {
    let ch = ctx.lib.live.get(channel)?;
    let p = crate::store::epg::programme(ctx.db, programme).ok()?;
    let shift = i64::from(ch.tvg_shift_min) * 60;
    let (start, stop) = (p.start + shift, p.stop + shift);
    let state = if start > ctx.now {
        Airing::NotYet
    } else if stop > ctx.now {
        Airing::Live
    } else if ch.has_catchup {
        Airing::Past { start, stop, catchup_id: p.catchup_id }
    } else {
        Airing::NoArchive
    };
    Some((state, ch.name.to_string()))
}

/// Play `target` as Shift+Enter plays it here (inside [`UiCtx::cast_with`]); a locked item asks
/// for the PIN with `Action::CastUnlocked`.
fn play(ctx: &mut UiCtx<'_>, target: &Target) {
    use crate::ui::screens::home::kit;
    use crate::ui::screens::live;
    let channel = |ctx: &mut UiCtx<'_>, id| {
        let zap = live::primary_zap(ctx, id);
        live::play_channel(ctx, id, zap, PlayOrigin::Fullscreen, Action::CastUnlocked);
    };
    match *target {
        Target::Channel(id) => channel(ctx, id),
        Target::Programme { programme, channel: id } => match airing(ctx, programme, id) {
            Some((Airing::Live, _)) => channel(ctx, id),
            Some((Airing::Past { start, stop, catchup_id }, name)) => {
                if kit::channel_locked(ctx, id) {
                    kit::ask_pin(ctx, Action::CastUnlocked, &format!("“{name}” is locked"));
                    return;
                }
                let media = MediaRef::Catchup { channel: id, start, stop, catchup_id };
                ctx.play(PlayerCommand::Play(PlayRequest { media, zap: None, origin: PlayOrigin::Guide, resume: false, immediate: true }));
            }
            _ => {}
        },
        Target::Movie(id) => title(ctx, VodKind::Movie(id)),
        Target::Series(id) => title(ctx, VodKind::Series(id)),
        Target::Episode { series, episode } => {
            let Some(idx) = ctx.lib.vod.index_of(VodKind::Series(series)) else { return };
            if vod_locked(ctx, false, idx) {
                kit::ask_pin(ctx, Action::CastUnlocked, "This series is locked");
                return;
            }
            let (Ok(plan), Ok(ep)) = (crate::ui::copies::plan(ctx, false, idx), crate::store::vod::episode(ctx.db, episode)) else { return };
            crate::ui::copies::play_episode(ctx, &plan, series, &ep, false, true);
        }
    }
}

/// A film or a series card: its own copy with the title's others as the fallback, behind the
/// Resume prompt when any copy can be resumed (a series: the episode its newest progress is at).
fn title(ctx: &mut UiCtx<'_>, kind: VodKind) {
    let movies = matches!(kind, VodKind::Movie(_));
    let Some(idx) = ctx.lib.vod.index_of(kind) else { return };
    if vod_locked(ctx, movies, idx) || !crate::ui::copies::play_title(ctx, movies, idx, Some(idx), true) {
        crate::ui::screens::home::kit::ask_pin(ctx, Action::CastUnlocked, "This title is locked");
    }
}

/// The card's copy (or one of its categories) is locked while the session is.
fn vod_locked(ctx: &UiCtx<'_>, movies: bool, idx: u32) -> bool {
    use crate::ui::screens::home::kit;
    let (items, groups) = if movies { (&ctx.lib.vod.movies, &ctx.lib.vod.movie_groups) } else { (&ctx.lib.vod.series, &ctx.lib.vod.series_groups) };
    let Some(v) = items.get(idx as usize) else { return false };
    (v.locked || kit::any_group_locked(groups, &v.group_ids)) && !kit::unlocked(ctx)
}

// ------------------------------------------------------------------ Play on (§3.9)

/// Picture A's snapshot (the switch and the cast are the main picture's, whichever has the
/// focus).
fn main_picture(ctx: &UiCtx<'_>) -> std::sync::Arc<PlayerSnapshot> {
    ctx.services.tiles.a().handle().snapshot()
}

/// Where plays go now: the TV picture A casts to, else the switch's (`None`: this computer).
pub fn plays_on(snap: &PlayerSnapshot) -> Option<DeviceId> {
    snap.cast.as_ref().map(|c| c.device).or(snap.play_on)
}

/// The chooser's choice: the switch goes there, and what plays follows it — something playing
/// here moves to the TV (the Live preview stops instead: it never casts), a cast moves to the
/// TV chosen, and "This computer" while casting is Play here. Nothing moves in multi-view (two
/// pictures are this computer's; the switch applies once it ends).
pub fn choose(ctx: &mut UiCtx<'_>, to: Option<DeviceId>) {
    let a = main_picture(ctx);
    ctx.play_in(TileId::A, PlayerCommand::SetPlayOn(to));
    let hex = to.map(|d| d.to_string());
    ctx.edit_config(move |c| c.cast.play_on = hex);
    let name = to.map(|id| ctx.services.agent.cast().devices().into_iter().find(|d| d.id == id).map_or_else(|| "your TV".to_owned(), |d| d.name));
    let here = a.cast.is_none() && (a.state.is_active() || a.state == PlayerState::Paused);
    let moved = match (to, ctx.multiview.on()) {
        (_, true) => false,
        (Some(d), false) if a.cast.as_ref().is_some_and(|c| c.device != d) => {
            ctx.play_in(TileId::A, PlayerCommand::CastTo { device: d, play: None });
            true
        }
        (Some(_), false) if here && a.origin == Some(PlayOrigin::LivePreview) => {
            ctx.play_in(TileId::A, PlayerCommand::Stop);
            false
        }
        (Some(d), false) if here => {
            ctx.play_in(TileId::A, PlayerCommand::CastTo { device: d, play: None });
            true
        }
        (None, false) if a.cast.is_some() => {
            ctx.play_in(TileId::A, PlayerCommand::StopCasting { play_here: true });
            true
        }
        _ => false,
    };
    if !moved && plays_on(&a) != to {
        ctx.toast(Toast::info(match name {
            Some(tv) => format!("What you play now plays on {tv}"),
            None => "What you play now plays on this computer".to_owned(),
        }));
    }
}

/// The status bar's chip text and accessible name for where plays go (`tv`: its name, `None`:
/// this computer), with the key that opens the chooser.
pub fn chip_text(tv: Option<&str>, key: &str) -> (String, String) {
    let (glyph, name) = match tv {
        Some(n) => (glyphs::CAST_CONNECTED, n),
        None => (glyphs::CAST, "This computer"),
    };
    (format!("{glyph} Play on: {name} {}", glyphs::CHEVRON_DOWN), format!("Play on: {name} — choose where plays go ({key})"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cast::{PairVariant, Protocol, Usability};
    use crate::model::ReceiverKind;
    use crate::ui::screens::home::testkit::rig;
    use crate::ui::{NavRequest, Route};

    /// [`UiCtx::cast_with`]: the screen stays (no Player route), a Resume prompt opened meanwhile
    /// carries the TV (its answer casts), and only a play that went out is toasted; outside it,
    /// nothing changes.
    #[test]
    fn cast_with_keeps_the_screen_and_hands_the_resume_prompt_its_tv() {
        let (mut t, _one) = rig();
        let dest = CastDest { device: DeviceId::from_bytes([1; 16]), tv: "Living Room".into(), title: "Dune".into() };
        let spec = crate::ui::overlays::ResumeSpec {
            media: MediaRef::Movie(crate::model::MovieId(1)),
            title: "Dune".into(),
            position_s: 600.0,
            duration_s: None,
            copy: None,
            queue: None,
            cast: None,
        };
        t.with_ctx(|ctx| {
            ctx.cast_with(dest.clone(), |ctx| {
                ctx.push(Route::Player);
                ctx.open(OverlayRequest::Resume(spec.clone()));
            })
        });
        let cast = crate::ui::overlays::ResumeSpec { cast: Some(dest.clone()), ..spec.clone() };
        assert_eq!(t.nav, [NavRequest::Open(OverlayRequest::Resume(cast))]);
        assert!(t.toasts.is_empty(), "nothing went out yet");
        t.nav.clear();
        let req = PlayRequest {
            media: MediaRef::Movie(crate::model::MovieId(1)),
            zap: None,
            origin: PlayOrigin::Fullscreen,
            resume: true,
            immediate: true,
        };
        t.with_ctx(|ctx| ctx.cast_with(dest.clone(), |ctx| ctx.play(PlayerCommand::Play(req))));
        assert_eq!(t.toasts.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(), ["Casting Dune to Living Room"]);
        t.with_ctx(|ctx| {
            ctx.push(Route::Player);
            ctx.open(OverlayRequest::Resume(spec.clone()));
        });
        assert_eq!(t.nav, [NavRequest::Push(Route::Player), NavRequest::Open(OverlayRequest::Resume(spec))]);
    }

    fn tv(n: u8, pairing: Pairing) -> Device {
        Device { pairing, ..Device::new(DeviceId::from_bytes([n; 16]), &format!("TV {n}"), ReceiverKind::AppleTv, Protocol::AirPlay) }
    }

    /// Only a TV that casts without a question counts for `C`'s "straight there": paired or
    /// needing no pairing, and usable; a remembered TV not seen this run still counts (its
    /// stored address is tried; a TV that's off gets the typed error card).
    #[test]
    fn ready_tvs_are_usable_and_paired() {
        let paired = tv(1, Pairing::Paired(PairVariant::Hkp3));
        let open = tv(2, Pairing::NotNeeded);
        let unpaired = tv(3, Pairing::Required);
        let mac = Device { usability: Usability::Mac, ..tv(4, Pairing::NotNeeded) };
        let away = Device { available: false, ..tv(5, Pairing::Paired(PairVariant::Hkp5)) };
        let all = [paired, open, unpaired, mac, away];
        let ids: Vec<u8> = ready(&all).iter().map(|d| d.id.as_bytes()[0]).collect();
        assert_eq!(ids, [1, 2, 5]);
    }
}
