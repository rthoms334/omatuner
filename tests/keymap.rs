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
    assert_eq!(resolve(&live, &key(Key::ArrowLeft)), Some(Action::LastChannel));
    assert_eq!(resolve(&live, &key(Key::ArrowRight)), Some(Action::ChannelHistory));
    let vod = [K::PlayerVod];
    assert_eq!(resolve(&vod, &key(Key::ArrowLeft)), Some(Action::SeekBack));
    assert_eq!(resolve(&vod, &shift(Key::ArrowRight)), Some(Action::SeekForwardBig));
    // controls focus is explicit: Enter, i or Tab
    for ev in [key(Key::Enter), text("i"), key(Key::Tab)] {
        assert_eq!(resolve(&live, &ev), Some(Action::FocusOsdControls), "{ev:?}");
        assert_eq!(resolve(&vod, &ev), Some(Action::FocusOsdControls), "{ev:?}");
    }
    // in controls focus mode arrows move between controls; Esc leaves; the rest falls through
    for base in [K::PlayerLive, K::PlayerVod] {
        let osd = [K::PlayerOsd, base];
        assert_eq!(resolve(&osd, &key(Key::ArrowLeft)), Some(Action::Left));
        assert_eq!(resolve(&osd, &key(Key::ArrowDown)), Some(Action::Down));
        assert_eq!(resolve(&osd, &key(Key::Enter)), Some(Action::Activate));
        assert_eq!(resolve(&osd, &key(Key::Escape)), Some(Action::LeaveOsdControls));
        assert_eq!(resolve(&osd, &key(Key::Space)), Some(Action::PlayPause));
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
    for ctx in [K::List, K::ListFiltering, K::PosterGrid, K::PosterGridFiltering, K::Search] {
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
    assert_eq!(resolve(&[K::Panel], &text("l")), Some(Action::Right));
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
/// ("Play from…") works in the grid, while filtering and on detail pages, and nowhere it would
/// shadow something (Ctrl+Shift+O stays the pop-out).
#[test]
fn stack_fan_out_and_play_from() {
    let ctrl_shift = |k: Key| key_mods(k, Modifiers::CTRL | Modifiers::COMMAND | Modifiers::SHIFT, false);
    assert_eq!(resolve(&[K::PosterGrid], &text(" ")), Some(Action::FanOut));
    assert_eq!(resolve(&[K::PosterGrid], &key(Key::Space)), None, "the key twin reaches nothing");
    assert_eq!(resolve(&[K::PosterGridFiltering], &text(" ")), Some(Action::FilterInput(' ')), "filters have spaces");
    assert_eq!(resolve(&[K::List], &text(" ")), Some(Action::ToggleMark), "group lists keep marking");
    for stack in [vec![K::PosterGrid], vec![K::PosterGridFiltering], vec![K::Detail], vec![K::PlayerVod]] {
        assert_eq!(resolve(&stack, &ctrl(Key::O)), Some(Action::PlayFrom), "{stack:?}");
        assert_eq!(resolve(&stack, &ctrl_shift(Key::O)), Some(Action::TogglePopOut), "{stack:?}");
    }
    assert_eq!(resolve(&[K::List], &ctrl(Key::O)), None, "channels have no copies");
    assert!(help_rows(K::PosterGrid).iter().any(|r| r.0 == "Space" && r.1.starts_with("Show each copy")));
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
fn help_is_complete_for_every_context() {
    let mut titles = Vec::new();
    for ctx in K::ALL {
        assert!(!ctx.title().is_empty());
        titles.push(ctx.title());
        let rows = help_rows(ctx);
        assert!(!rows.is_empty(), "{ctx:?} has no help rows");
        // every binding of the context appears with its key in the row for its help text
        for t in tables(ctx) {
            for b in t.iter() {
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
    assert_eq!(hint_line(&[K::Guide]), "Enter details · w watch · r remind · n now · ? help");
    // the innermost context with hints wins; contexts without hints are skipped
    assert_eq!(hint_line(&[K::PlayerOsd, K::List]), hint_line(&[K::List]));
    assert!(hint_line(&[K::Modal]).starts_with("Enter confirm"));
    assert_eq!(hint_line(&[]), "");
}
