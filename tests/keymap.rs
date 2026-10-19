//! Keymap resolution and generated help (DESIGN §15). Owner: WP-10a.
//!
//! Exercises `keymap::resolve` through the public API with the context stacks the screens
//! really build (side panels over the player, numeric entry, text fields over dialogs...).

use eframe::egui::{self, Key, Modifiers};
use tuner::keymap::{self, Action, KeyChord, KeyContext as K, Trigger, help_rows, hint_line, resolve, tables};

fn key_mods(k: Key, modifiers: Modifiers, repeat: bool) -> egui::Event {
    egui::Event::Key { key: k, physical_key: None, pressed: true, repeat, modifiers }
}
fn key(k: Key) -> egui::Event {
    key_mods(k, Modifiers::NONE, false)
}
fn ctrl(k: Key) -> egui::Event {
    key_mods(k, Modifiers::CTRL | Modifiers::COMMAND, false)
}
fn alt(k: Key) -> egui::Event {
    key_mods(k, Modifiers::ALT, false)
}
fn shift(k: Key) -> egui::Event {
    key_mods(k, Modifiers::SHIFT, false)
}
fn held(k: Key) -> egui::Event {
    key_mods(k, Modifiers::NONE, true)
}
fn text(s: &str) -> egui::Event {
    egui::Event::Text(s.into())
}

// ------------------------------------------------------------------ layering

#[test]
fn innermost_context_wins_and_unbound_keys_fall_through() {
    // player side panel: the list moves, Space still pauses, letters filter the panel
    let panel = [K::List, K::PlayerLive];
    assert_eq!(resolve(&panel, &key(Key::ArrowDown)), Some(Action::Down));
    assert_eq!(resolve(&panel, &key(Key::Space)), Some(Action::PlayPause));
    assert_eq!(resolve(&panel, &text("j")), Some(Action::FilterInput('j')));
    assert_eq!(resolve(&panel, &key(Key::Escape)), Some(Action::Back));
    // numeric entry over the player: digits and Enter are its own, arrows still zap
    let numeric = [K::NumericEntry, K::PlayerLive];
    assert_eq!(resolve(&numeric, &text("4")), Some(Action::Digit(4)));
    assert_eq!(resolve(&numeric, &key(Key::Enter)), Some(Action::Activate));
    assert_eq!(resolve(&numeric, &key(Key::Escape)), Some(Action::Back));
    assert_eq!(resolve(&numeric, &key(Key::ArrowUp)), Some(Action::ZapPrev));
    assert_eq!(resolve(&numeric, &key(Key::Space)), Some(Action::PlayPause));
    // numeric entry over the guide
    assert_eq!(resolve(&[K::NumericEntry, K::Guide], &text("j")), Some(Action::Down));
    // EPG mapping stacked over a filtering list
    let mapping = [K::EpgMapping, K::ListFiltering];
    assert_eq!(resolve(&mapping, &key(Key::Enter)), Some(Action::Activate));
    assert_eq!(resolve(&mapping, &key(Key::Delete)), Some(Action::ClearEpg));
    assert_eq!(resolve(&mapping, &key(Key::Backspace)), Some(Action::FilterBackspace));
    assert_eq!(resolve(&mapping, &text("a")), Some(Action::FilterInput('a')));
}

#[test]
fn globals_reach_every_stack_but_never_take_typing() {
    for stack in [
        vec![K::List],
        vec![K::ListFiltering],
        vec![K::Guide],
        vec![K::PlayerOsd, K::PlayerLive],
        vec![K::Modal],
        vec![K::Search],
        vec![K::Panel],
        vec![K::TextInput, K::Modal],
    ] {
        assert_eq!(resolve(&stack, &ctrl(Key::Q)), Some(Action::Quit), "{stack:?}");
        assert_eq!(resolve(&stack, &alt(Key::Num3)), Some(Action::GoGuide), "{stack:?}");
        assert_eq!(resolve(&stack, &ctrl(Key::K)), Some(Action::OpenSearch), "{stack:?}");
        assert_eq!(resolve(&stack, &key(Key::F1)), Some(Action::ToggleHelp), "{stack:?}");
    }
    for b in keymap::GLOBAL {
        let ok = match b.chord.trigger {
            Trigger::Key(k) => b.chord.ctrl || b.chord.alt || k.name().starts_with('F') && k.name().len() > 1,
            Trigger::Char(_) => false,
        };
        assert!(ok, "Global binding {} would steal typing", b.chord.label());
    }
    // an empty stack still has the globals
    assert_eq!(resolve(&[], &key(Key::F11)), Some(Action::ToggleWindowFullscreen));
    assert_eq!(resolve(&[], &text("q")), None);
}

#[test]
fn modal_swallows_text_it_does_not_bind() {
    assert_eq!(resolve(&[K::Modal], &text("a")), None);
    assert_eq!(resolve(&[K::Modal, K::PlayerLive], &text("m")), None, "contexts below a dialog never see letters");
    assert_eq!(resolve(&[K::Modal], &key(Key::Space)), Some(Action::Activate));
    assert_eq!(resolve(&[K::Modal], &key(Key::Escape)), Some(Action::Back));
    // a filterable menu stacked above Modal filters
    assert_eq!(resolve(&[K::List, K::Modal], &text("a")), Some(Action::FilterInput('a')));
    assert_eq!(resolve(&[K::ListFiltering, K::Modal], &key(Key::Escape)), Some(Action::FilterClear));
}

// ------------------------------------------------------------------ text fields

#[test]
fn text_input_owns_the_keyboard() {
    let field = [K::TextInput, K::List];
    for ev in [
        key(Key::Space),
        key(Key::ArrowLeft),
        key(Key::ArrowRight),
        key(Key::ArrowUp),
        key(Key::Delete),
        key(Key::Backspace),
        key(Key::Home),
        key(Key::End),
        text(" "),
        text("j"),
        text("?"),
        text("/"),
        shift(Key::ArrowLeft),
    ] {
        assert_eq!(resolve(&field, &ev), None, "{ev:?} must reach the text widget");
    }
    // its own table
    assert_eq!(resolve(&field, &key(Key::Enter)), Some(Action::Activate));
    assert_eq!(resolve(&field, &key(Key::Escape)), Some(Action::Back));
    assert_eq!(resolve(&field, &key(Key::Tab)), Some(Action::FocusNext));
    assert_eq!(resolve(&field, &shift(Key::Tab)), Some(Action::FocusPrev));
    // chords and F-keys reach Global only — never the list below the field
    assert_eq!(resolve(&field, &ctrl(Key::Q)), Some(Action::Quit));
    assert_eq!(resolve(&field, &alt(Key::Num2)), Some(Action::GoLive));
    assert_eq!(resolve(&field, &ctrl(Key::F)), None, "Ctrl+F is an item action of the list below");
    assert_eq!(resolve(&field, &key(Key::F2)), None, "F2 (rename) belongs to the list below");
    assert_eq!(resolve(&field, &ctrl(Key::Backspace)), None, "word delete is the text widget's");
}

// ------------------------------------------------------------------ repeat

#[test]
fn auto_repeat_never_steps_out_of_a_layer() {
    assert_eq!(resolve(&[K::List], &held(Key::Backspace)), None);
    assert_eq!(resolve(&[K::List], &held(Key::Escape)), None);
    assert_eq!(resolve(&[K::List], &held(Key::ArrowLeft)), None);
    assert_eq!(resolve(&[K::Detail], &held(Key::Backspace)), None);
    assert_eq!(resolve(&[K::Guide], &key_mods(Key::ArrowLeft, Modifiers::ALT, true)), None);
    // holding Backspace clears the filter character by character, then stops
    assert_eq!(resolve(&[K::ListFiltering], &held(Key::Backspace)), Some(Action::FilterBackspace));
    // held navigation keys keep repeating
    assert_eq!(resolve(&[K::List], &held(Key::ArrowDown)), Some(Action::Down));
    assert_eq!(resolve(&[K::PlayerLive], &held(Key::ArrowDown)), Some(Action::ZapNext));
    assert_eq!(resolve(&[K::PlayerVod], &held(Key::ArrowRight)), Some(Action::SeekForward));
}

// ------------------------------------------------------------------ player: banner vs controls

#[test]
fn info_banner_never_changes_the_keys_only_controls_focus_does() {
    // passive banner = plain PlayerLive/Vod stack: arrows always zap / recall / seek
    let live = [K::PlayerLive];
    assert_eq!(resolve(&live, &key(Key::ArrowUp)), Some(Action::ZapPrev));
    assert_eq!(resolve(&live, &key(Key::ArrowDown)), Some(Action::ZapNext));
    assert_eq!(resolve(&live, &key(Key::ArrowLeft)), Some(Action::SeekBack));
    assert_eq!(resolve(&live, &key(Key::ArrowRight)), Some(Action::SeekForward));
    assert_eq!(resolve(&live, &shift(Key::ArrowLeft)), Some(Action::SeekBackBig));
    assert_eq!(resolve(&live, &shift(Key::ArrowRight)), Some(Action::SeekForwardBig));
    assert_eq!(resolve(&live, &text("H")), Some(Action::ChannelHistory));
    assert_eq!(resolve(&live, &text("h")), None);
    assert_eq!(resolve(&live, &key(Key::Backspace)), Some(Action::LastChannel));
    assert_eq!(resolve(&live, &text("L")), Some(Action::JumpToLive));
    let vod = [K::PlayerVod];
    assert_eq!(resolve(&vod, &key(Key::ArrowLeft)), Some(Action::SeekBack));
    assert_eq!(resolve(&vod, &shift(Key::ArrowRight)), Some(Action::SeekForwardBig));
    // controls focus is explicit: Enter, i or Tab
    for ev in [key(Key::Enter), text("i"), key(Key::Tab)] {
        assert_eq!(resolve(&live, &ev), Some(Action::FocusOsdControls), "{ev:?}");
        assert_eq!(resolve(&vod, &ev), Some(Action::FocusOsdControls), "{ev:?}");
    }
    // in controls focus mode arrows move, Enter / Space activate, Esc leaves; the rest falls through
    for base in [K::PlayerLive, K::PlayerVod] {
        let osd = [K::PlayerOsd, base];
        assert_eq!(resolve(&osd, &key(Key::ArrowLeft)), Some(Action::Left));
        assert_eq!(resolve(&osd, &key(Key::ArrowDown)), Some(Action::Down));
        assert_eq!(resolve(&osd, &key(Key::Enter)), Some(Action::Activate));
        assert_eq!(resolve(&osd, &key(Key::Escape)), Some(Action::LeaveOsdControls));
        assert_eq!(resolve(&osd, &key(Key::Space)), Some(Action::Activate));
        assert_eq!(resolve(&[base], &key(Key::Space)), Some(Action::PlayPause));
        assert_eq!(resolve(&osd, &text("m")), Some(Action::Mute));
    }
    // Esc on the plain player is Back (the app's Esc ladder hides the banner first)
    assert_eq!(resolve(&live, &key(Key::Escape)), Some(Action::Back));
    assert_eq!(resolve(&live, &text("7")), Some(Action::Digit(7)));
    assert_eq!(resolve(&vod, &text("7")), None, "no channel numbers in VOD");
}

// ------------------------------------------------------------------ filterable contexts

#[test]
fn letters_filter_lists_and_grids_but_drive_guide_player_and_panels() {
    for ctx in [K::List, K::ListFiltering, K::PosterGrid, K::PosterGridFiltering, K::Search, K::Settings] {
        for c in ['a', 'j', 'k', 'Z', 'é', '1', 'ß'] {
            assert_eq!(resolve(&[ctx], &text(&c.to_string())), Some(Action::FilterInput(c)), "{ctx:?} {c}");
        }
        // no vim keys (or any letter) bound in filterable tables
        for t in tables(ctx) {
            for b in t.iter() {
                if let Trigger::Char(c) = b.chord.trigger {
                    assert!(!c.is_alphanumeric(), "{ctx:?} binds '{c}', which must type");
                }
            }
        }
    }
    assert_eq!(resolve(&[K::Guide], &text("k")), Some(Action::Up));
    assert_eq!(resolve(&[K::Settings], &text("l")), Some(Action::FilterInput('l')));
    assert_eq!(resolve(&[K::PlayerLive], &text("k")), Some(Action::ZapPrev));
    // IME commits of several characters and control characters are not filter input
    assert_eq!(resolve(&[K::List], &text("ab")), None);
    assert_eq!(resolve(&[K::List], &text("\u{7f}")), None);
    // '/' searches until a filter is active, then it types
    assert_eq!(resolve(&[K::List], &text("/")), Some(Action::OpenSearch));
    assert_eq!(resolve(&[K::ListFiltering], &text("/")), Some(Action::FilterInput('/')));
}

#[test]
fn list_parent_and_filter_keys() {
    assert_eq!(resolve(&[K::List], &key(Key::ArrowLeft)), Some(Action::Parent));
    assert_eq!(resolve(&[K::List], &key(Key::Backspace)), Some(Action::Parent));
    assert_eq!(resolve(&[K::List], &key(Key::ArrowRight)), Some(Action::Activate));
    assert_eq!(resolve(&[K::List], &shift(Key::Enter)), Some(Action::ActivateAlt));
    assert_eq!(resolve(&[K::ListFiltering], &ctrl(Key::Backspace)), Some(Action::FilterDeleteWord));
    assert_eq!(resolve(&[K::ListFiltering], &ctrl(Key::U)), Some(Action::FilterClear));
    assert_eq!(resolve(&[K::ListFiltering], &key(Key::Escape)), Some(Action::FilterClear));
    // item actions work while filtering
    assert_eq!(resolve(&[K::ListFiltering], &ctrl(Key::F)), Some(Action::ToggleFavourite));
    assert_eq!(resolve(&[K::List], &shift(Key::F10)), Some(Action::ContextMenu));
    assert_eq!(resolve(&[K::List], &ctrl(Key::Enter)), Some(Action::ContextMenu));
    assert_eq!(resolve(&[K::List], &key(Key::Delete)), Some(Action::Remove));
    // modifiers must match exactly
    assert_eq!(resolve(&[K::List], &shift(Key::ArrowDown)), None);
    assert_eq!(resolve(&[K::List], &ctrl(Key::ArrowDown)), None);
}

/// Bulk hide in group panes and the "Choose what to show" tick list (DESIGN §28.7): Space marks
/// only while the filter is empty (then it types), Insert always marks; "hide these N groups"
/// only exists while filtering. The tick list owns its keys above `Modal`: Space ticks instead
/// of confirming the dialog, → doesn't save and Backspace doesn't leave (as `List` would).
#[test]
fn group_marks_and_tick_lists() {
    let ctrl_shift = |k: Key| key_mods(k, Modifiers::CTRL | Modifiers::COMMAND | Modifiers::SHIFT, false);
    assert_eq!(resolve(&[K::List], &text(" ")), Some(Action::ToggleMark));
    assert_eq!(resolve(&[K::List], &key(Key::Space)), None, "the key twin reaches nothing");
    assert_eq!(resolve(&[K::List], &key(Key::Insert)), Some(Action::ToggleMark));
    assert_eq!(resolve(&[K::ListFiltering], &text(" ")), Some(Action::FilterInput(' ')), "filters have spaces");
    assert_eq!(resolve(&[K::ListFiltering], &key(Key::Insert)), Some(Action::ToggleMark));
    assert_eq!(resolve(&[K::ListFiltering], &ctrl_shift(Key::H)), Some(Action::HideShown));
    assert_eq!(resolve(&[K::List], &ctrl_shift(Key::H)), None, "only while filtering");
    assert_eq!(resolve(&[K::List], &ctrl(Key::H)), Some(Action::ToggleHidden));

    let picker = [K::Checklist, K::Modal];
    assert_eq!(resolve(&picker, &key(Key::Space)), Some(Action::ToggleMark), "not the dialog's Confirm");
    assert_eq!(resolve(&picker, &text(" ")), Some(Action::FilterInput(' ')), "ignored by tick lists");
    assert_eq!(resolve(&picker, &text("f")), Some(Action::FilterInput('f')));
    assert_eq!(resolve(&picker, &held(Key::Escape)), None);
    assert_eq!(resolve(&picker, &key(Key::ArrowRight)), Some(Action::Right), "the dialog's harmless →, never Activate");
    assert_eq!(hint_line(&picker), "Space tick · Ctrl+A all/none · Ctrl+E my country · Enter save");
    assert!(help_rows(K::Checklist).iter().any(|r| r.1 == "Type to filter"));
}

/// Stacks of copies (DESIGN §29.14): Space fans a stack out in the movie / series grid only
/// while nothing is typed (then it types, like the group marks); it never marks there. Ctrl+O
/// ("Play from…") works in the grid, while filtering and on detail pages — and in lists, where
/// Live TV offers the same channel in another playlist ("Also on", §29.16) — and nowhere it
/// would shadow something (Ctrl+Shift+O stays the pop-out).
#[test]
fn stack_fan_out_and_play_from() {
    let ctrl_shift = |k: Key| key_mods(k, Modifiers::CTRL | Modifiers::COMMAND | Modifiers::SHIFT, false);
    assert_eq!(resolve(&[K::PosterGrid], &text(" ")), Some(Action::FanOut));
    assert_eq!(resolve(&[K::PosterGrid], &key(Key::Space)), None, "the key twin reaches nothing");
    assert_eq!(resolve(&[K::PosterGridFiltering], &text(" ")), Some(Action::FilterInput(' ')), "filters have spaces");
    assert_eq!(resolve(&[K::List], &text(" ")), Some(Action::ToggleMark), "group lists keep marking");
    for stack in [vec![K::PosterGrid], vec![K::PosterGridFiltering], vec![K::Detail], vec![K::PlayerVod], vec![K::List], vec![K::ListFiltering]] {
        assert_eq!(resolve(&stack, &ctrl(Key::O)), Some(Action::PlayFrom), "{stack:?}");
        assert_eq!(resolve(&stack, &ctrl_shift(Key::O)), Some(Action::TogglePopOut), "{stack:?}");
    }
    assert!(help_rows(K::PosterGrid).iter().any(|r| r.0 == "Space" && r.1.starts_with("Show each copy")));
}

// ------------------------------------------------------------------ the big-features batch

fn ctrl_shift(k: Key) -> egui::Event {
    key_mods(k, Modifiers::CTRL | Modifiers::COMMAND | Modifiers::SHIFT, false)
}

/// Up next (DESIGN §32): Ctrl+N / Ctrl+Shift+N on every item, search hit and guide programme —
/// never Global (a text field would pass it on) — Alt+6 everywhere, u / n / N in both players.
#[test]
fn up_next_keys() {
    for stack in
        [vec![K::List], vec![K::ListFiltering], vec![K::PosterGrid], vec![K::PosterGridFiltering], vec![K::Detail], vec![K::Search], vec![K::Guide]]
    {
        assert_eq!(resolve(&stack, &ctrl(Key::N)), Some(Action::QueueAdd), "{stack:?}");
        assert_eq!(resolve(&stack, &ctrl_shift(Key::N)), Some(Action::QueuePlayNext), "{stack:?}");
    }
    assert!(!keymap::GLOBAL.iter().any(|b| matches!(b.action, Action::QueueAdd | Action::QueuePlayNext)));
    assert_eq!(resolve(&[K::TextInput, K::List], &ctrl(Key::N)), None, "a text field never adds");
    for stack in [vec![K::List], vec![K::PlayerLive], vec![K::TextInput, K::Panel]] {
        assert_eq!(resolve(&stack, &alt(Key::Num6)), Some(Action::GoQueue), "{stack:?}");
    }
    for player in [K::PlayerLive, K::PlayerVod] {
        assert_eq!(resolve(&[player], &text("u")), Some(Action::QueuePanel));
        assert_eq!(resolve(&[player], &text("n")), Some(Action::QueueNext));
        assert_eq!(resolve(&[player], &text("N")), Some(Action::QueuePrev));
        // a side panel's filter still types them
        assert_eq!(resolve(&[K::List, player], &text("n")), Some(Action::FilterInput('n')));
    }
}

/// Following, the spoiler guard and the sleep timer: R next to r in the guide; Ctrl+B /
/// Ctrl+Shift+B / Ctrl+Shift+T anywhere, text fields and filters included; Ctrl+W marks a
/// programme seen.
#[test]
fn follow_spoiler_and_sleep_keys() {
    assert_eq!(resolve(&[K::Guide], &text("R")), Some(Action::FollowProgramme));
    assert_eq!(resolve(&[K::Guide], &text("r")), Some(Action::GuideRemind));
    assert_eq!(resolve(&[K::Guide], &ctrl(Key::W)), Some(Action::MarkWatched));
    for stack in [vec![K::ListFiltering], vec![K::TextInput, K::Modal], vec![K::Guide], vec![K::PlayerLive], vec![K::Search]] {
        assert_eq!(resolve(&stack, &ctrl(Key::B)), Some(Action::RevealSpoilers), "{stack:?}");
        assert_eq!(resolve(&stack, &ctrl_shift(Key::B)), Some(Action::SpoilerMenu), "{stack:?}");
        assert_eq!(resolve(&stack, &ctrl_shift(Key::T)), Some(Action::SleepTimer), "{stack:?}");
    }
}

/// Multi-view (DESIGN §35): its keys above the focused picture's context; unbound keys fall
/// through; `q` closes a picture only here; Esc keeps its ladder; side panels still filter.
#[test]
fn multi_view_keys() {
    let mv = [K::MultiView, K::PlayerLive];
    assert_eq!(resolve(&mv, &text("w")), Some(Action::NextPicture));
    assert_eq!(resolve(&mv, &text("M")), Some(Action::SoundHere));
    assert_eq!(resolve(&mv, &text("m")), Some(Action::Mute), "m stays mute");
    assert_eq!(resolve(&mv, &text("W")), Some(Action::SwapPictures));
    assert_eq!(resolve(&mv, &text("t")), Some(Action::CycleLayout));
    assert_eq!(resolve(&mv, &text("q")), Some(Action::ClosePicture));
    assert_eq!(resolve(&[K::PlayerLive], &text("q")), Some(Action::StopAndClose), "single view unchanged");
    assert_eq!(resolve(&mv, &ctrl(Key::ArrowRight)), Some(Action::FocusPictureRight));
    assert_eq!(resolve(&mv, &key(Key::ArrowDown)), Some(Action::ZapNext), "zaps the focused picture");
    assert_eq!(resolve(&mv, &key(Key::Escape)), Some(Action::Back), "Esc never closes a picture");
    assert_eq!(resolve(&[K::List, K::MultiView, K::PlayerLive], &text("w")), Some(Action::FilterInput('w')));
    assert_eq!(resolve(&[K::PopOut, K::MultiView, K::PlayerVod], &text("W")), Some(Action::SwapPictures));
    assert_eq!(resolve(&[K::PlayerOsd, K::MultiView, K::PlayerLive], &text("t")), Some(Action::CycleLayout));
    // Ctrl+M: from lists, the guide, search and the player (never Global)
    for stack in [vec![K::List], vec![K::Guide], vec![K::Search], vec![K::PlayerLive], vec![K::PlayerVod], mv.to_vec()] {
        assert_eq!(resolve(&stack, &ctrl(Key::M)), Some(Action::WatchAlongside), "{stack:?}");
    }
    assert_eq!(resolve(&[K::TextInput, K::Modal], &ctrl(Key::M)), None);
    if tuner::ui::multiview::ENABLED {
        assert_eq!(hint_line(&mv), "w next picture · M sound here · W swap · t layout · q close picture");
    } else {
        assert_eq!(hint_line(&mv), hint_line(&[K::PlayerLive]), "multi-view's hints wait for the feature");
    }
}

/// Guide fixes (DESIGN §31) on the mapping screen only; Delete ends a news flash in the
/// pop-out.
#[test]
fn guide_fix_and_news_flash_keys() {
    let map = [K::EpgMapping, K::List];
    assert_eq!(resolve(&map, &shift(Key::Enter)), Some(Action::AcceptFix));
    assert_eq!(resolve(&[K::List], &shift(Key::Enter)), Some(Action::ActivateAlt), "other lists keep fullscreen");
    assert_eq!(resolve(&map, &ctrl(Key::A)), Some(Action::MarkAll));
    assert_eq!(resolve(&map, &ctrl(Key::Z)), Some(Action::UndoGuideFix(None)));
    assert_eq!(resolve(&map, &ctrl_shift(Key::ArrowRight)), Some(Action::ShiftLater));
    assert_eq!(resolve(&[K::EpgMapping, K::ListFiltering], &ctrl_shift(Key::ArrowLeft)), Some(Action::ShiftEarlier));
    assert_eq!(resolve(&[K::EpgMapping, K::ListFiltering], &text(" ")), Some(Action::FilterInput(' ')), "filters keep their spaces");
    assert_eq!(resolve(&[K::PopOut, K::PlayerLive], &key(Key::Delete)), Some(Action::EndNewsFlash));
}

// ------------------------------------------------------------------ tables, help, hints

#[test]
fn no_binding_is_shadowed_inside_a_context() {
    for ctx in K::ALL {
        let mut seen: Vec<KeyChord> = Vec::new();
        for t in tables(ctx) {
            for b in t.iter() {
                // the same chord twice for the same action is an alias table entry; for a
                // different action the second one could never fire
                if let Some(prev) = seen.iter().position(|c| *c == b.chord) {
                    let first = tables(ctx).iter().flat_map(|t| t.iter()).find(|x| x.chord == seen[prev]).map(|x| x.action);
                    assert_eq!(first, Some(b.action), "{ctx:?}: {} is bound twice", b.chord.label());
                }
                seen.push(b.chord);
            }
        }
    }
}

#[test]
fn lip_sync_keys_reach_both_players_and_the_pop_out_without_stealing_typing() {
    for player in [K::PlayerLive, K::PlayerVod] {
        for mut stack in [vec![], vec![K::PopOut], vec![K::PlayerOsd], vec![K::PopOut, K::MultiView], vec![K::NumericEntry]] {
            stack.push(player);
            assert_eq!(resolve(&stack, &text("[")), Some(Action::LipSyncEarlier), "{stack:?}");
            assert_eq!(resolve(&stack, &text("]")), Some(Action::LipSyncLater), "{stack:?}");
        }
        assert_eq!(resolve(&[K::List, player], &text("[")), Some(Action::FilterInput('[')));
        assert_eq!(resolve(&[K::TextInput, player], &text("]")), None);
    }
    assert_eq!(resolve(&[K::Guide], &text("[")), Some(Action::GuidePrevDay));
    assert_eq!(resolve(&[K::Guide], &text("]")), Some(Action::GuideNextDay));
}

#[test]
fn help_is_complete_for_every_context() {
    let mut titles = Vec::new();
    for ctx in K::ALL {
        assert!(!ctx.title().is_empty());
        titles.push(ctx.title());
        let rows = help_rows(ctx);
        // a context of a feature that is off (multi-view) lists nothing yet
        let on = tables(ctx).iter().any(|t| t.iter().any(|b| tuner::keymap::feature_on(b.action)));
        assert_eq!(rows.is_empty(), !on, "{ctx:?}");
        // every binding of the context appears with its key in the row for its help text,
        // once its feature is on
        for t in tables(ctx) {
            for b in t.iter().filter(|b| tuner::keymap::feature_on(b.action)) {
                let row = rows.iter().find(|(_, h)| *h == b.help).unwrap_or_else(|| panic!("{ctx:?}: '{}' missing", b.help));
                assert!(row.0.split(" / ").any(|k| k == b.chord.label()), "{ctx:?}: {} missing in '{}'", b.chord.label(), row.0);
            }
        }
        // rows are unique by text
        let mut helps: Vec<_> = rows.iter().map(|r| r.1).collect();
        helps.sort();
        helps.dedup();
        assert_eq!(helps.len(), rows.len(), "{ctx:?} repeats a help row");
    }
    titles.sort();
    titles.dedup();
    assert_eq!(titles.len(), K::ALL.len(), "context titles must be unique");
    // filterable lists document type-to-filter; search is always typing
    assert!(help_rows(K::List).iter().any(|r| r.1 == "Type to filter"));
    assert!(!help_rows(K::Search).iter().any(|r| r.1 == "Type to filter"));
}

/// Search's help lists only what the overlay does: no reorder, sort or remove there (they
/// used to come with the shared item table and answer "not here").
#[test]
fn search_binds_only_the_item_shortcuts_it_acts_on() {
    for ev in [alt(Key::ArrowUp), alt(Key::ArrowDown), ctrl(Key::S), key(Key::Delete)] {
        assert_eq!(resolve(&[K::Search], &ev), None, "{ev:?}");
    }
    assert_eq!(resolve(&[K::Search], &ctrl(Key::G)), Some(Action::OpenInGuide));
    assert_eq!(resolve(&[K::Search], &ctrl(Key::F)), Some(Action::ToggleFavourite));
    let rows = help_rows(K::Search);
    assert!(!rows.iter().any(|(_, h)| h.contains("reorder") || h.starts_with("Sort") || h.starts_with("Remove")), "{rows:?}");
}

#[test]
fn labels_are_human() {
    assert_eq!(KeyChord::ctrl(Key::K).label(), "Ctrl+K");
    assert_eq!(KeyChord::shift(Key::Enter).label(), "Shift+Enter");
    assert_eq!(KeyChord::ctrl_shift(Key::P).label(), "Ctrl+Shift+P");
    assert_eq!(KeyChord::alt(Key::ArrowLeft).label(), "Alt+←");
    assert_eq!(KeyChord::ch('?').label(), "?");
    assert_eq!(KeyChord::ch(' ').label(), "Space");
    assert_eq!(KeyChord::key(Key::PageDown).label(), "PgDn");
}

#[test]
fn hint_line_comes_from_the_tables() {
    assert_eq!(hint_line(&[K::List]), "Enter play · Shift+Enter fullscreen · Ctrl+F fav · Shift+F10 more · ? help");
    let follow = if tuner::follow::ENABLED { " · R follow" } else { "" };
    assert_eq!(hint_line(&[K::Guide]), format!("Enter details · w watch · r remind{follow} · ? help"));
    // the innermost context with hints wins; contexts without hints are skipped
    assert_eq!(hint_line(&[K::PlayerOsd, K::List]), hint_line(&[K::List]));
    assert!(hint_line(&[K::Modal]).starts_with("Enter confirm"));
    assert_eq!(hint_line(&[]), "");
}

#[test]
fn epg_mapping_adjust_keeps_panel_shortcuts() {
    let stack = [K::EpgMapping, K::Panel];
    assert_eq!(resolve(&stack, &key(Key::Space)), Some(Action::Activate));
    for (ch, action) in [
        ('j', Action::Down),
        ('k', Action::Up),
        ('h', Action::Left),
        ('l', Action::Right),
        ('H', Action::Decrease),
        ('L', Action::Increase),
        ('J', Action::MoveDown),
        ('K', Action::MoveUp),
        ('x', Action::ContextMenu),
        ('?', Action::ToggleHelp),
    ] {
        assert_eq!(resolve(&stack, &text(&ch.to_string())), Some(action));
        assert_eq!(resolve(&[K::Settings], &text(&ch.to_string())), Some(Action::FilterInput(ch)));
    }
    assert_eq!(resolve(&stack, &text("z")), None, "Adjust does not filter");
}

#[test]
fn player_settings_keys_keep_the_existing_player_shortcuts() {
    for base in [K::PlayerLive, K::PlayerVod] {
        for extra in [vec![], vec![K::PlayerOsd], vec![K::PopOut], vec![K::PopOut, K::MultiView]] {
            let mut stack = extra;
            stack.push(base);
            assert_eq!(resolve(&stack, &text("O")), Some(Action::PlayerSettings), "{stack:?}");
            assert_eq!(resolve(&stack, &text("o")), Some(Action::TogglePopOut));
            assert_eq!(resolve(&stack, &text("I")), Some(Action::StreamInfo));
            stack.insert(0, K::PlayerSettings);
            for k in [Key::Escape, Key::ArrowLeft, Key::Backspace] {
                assert_eq!(resolve(&stack, &key(k)), Some(Action::Back), "{stack:?}: {k:?}");
            }
            for k in [Key::Enter, Key::Space] {
                assert_eq!(resolve(&stack, &key(k)), Some(Action::Activate));
            }
            assert_eq!(resolve(&stack, &key(Key::ArrowUp)), Some(Action::Up));
            assert_eq!(resolve(&stack, &key(Key::ArrowDown)), Some(Action::Down));
            assert_eq!(resolve(&stack, &key(Key::ArrowRight)), Some(Action::Right));
            for (key, action) in [
                ("A", Action::AudioMenu),
                ("S", Action::SubtitleMenu),
                ("[", Action::LipSyncEarlier),
                ("]", Action::LipSyncLater),
                ("z", Action::SubDelayMinus),
                ("x", Action::SubDelayPlus),
                ("Z", Action::SubDelayReset),
            ] {
                assert_eq!(resolve(&stack, &text(key)), Some(action));
            }
        }
    }
    assert_eq!(resolve(&[K::PlayerLive], &text("H")), Some(Action::ChannelHistory));
    assert_eq!(resolve(&[K::PlayerLive], &text("i")), Some(Action::FocusOsdControls));
}
