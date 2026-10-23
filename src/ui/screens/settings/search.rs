//! Search metadata is derived from the rows the pages actually render. There is no parallel
//! catalogue: adding a row makes it searchable, and the page's visibility checks apply first.
use super::*;
use crate::search::SettingDocument;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingTarget {
    pub page: SettingsPage,
    pub title: String,
    key: SettingKey,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SettingKey {
    Control(String),
    Item(Item),
    Action(Act),
    Local(Local),
    AudioOutput(String, bool),
}

impl SettingTarget {
    pub fn page_name(&self) -> &'static str {
        PAGES_ALL.iter().find(|p| p.0 == self.page).map_or("Settings", |p| p.2)
    }
}

#[derive(Clone)]
pub(crate) struct IndexedRow {
    pub target: SettingTarget,
    pub document: SettingDocument,
    row: Row,
    position: usize,
}

impl IndexedRow {
    /// Global search finds settings, never programme titles, hidden items or other user data.
    pub(crate) fn is_setting(&self) -> bool {
        !matches!(self.row, Row::Item { .. })
    }
}

/// Missing items have no trustworthy lock metadata in the snapshot, so only an unlocked
/// session may search their saved names. Hidden VOD and orphans are included in that case.
pub(super) fn searchable_item(ctx: &UiCtx<'_>, item: &ItemRef) -> bool {
    if ctx.lib.conceals(item) {
        return false;
    }
    if kit::unlocked(ctx) {
        return true;
    }
    if item.kind == ItemKind::Channel {
        return ctx.lib.live.by_ref(item).is_some_and(|id| !kit::channel_locked(ctx, id));
    }
    let (items, groups) = if item.kind == ItemKind::Movie {
        (&ctx.lib.vod.movies, &ctx.lib.vod.movie_groups)
    } else {
        (&ctx.lib.vod.series, &ctx.lib.vod.series_groups)
    };
    items.iter().find(|v| v.source_id == item.source && v.key == item.key).is_some_and(|v| !v.locked && !kit::any_group_locked(groups, &v.group_ids))
}

impl Row {
    fn title(&self) -> &str {
        match self {
            Self::Toggle(r) => r.label,
            Self::Choice(r) => r.label,
            Self::Number(r) => r.label,
            Self::Text(r) => r.label,
            Self::LipSync { reset: true, .. } => "Reset audio delay",
            Self::LipSync { .. } => "Audio delay",
            Self::Local { label, .. } => label,
            Self::Act { label, .. } | Self::Item { label, .. } => label,
            Self::Section(s) | Self::Note(s, _) => s,
        }
    }
}

/// Short aliases supplement the row's own title and visible explanatory copy. Unlisted rows
/// still match automatically by title, page, section and description.
fn words(page: SettingsPage, title: &str) -> Vec<&'static str> {
    let mut words = match page {
        SettingsPage::Playlists => vec!["source", "provider", "m3u", "xtream"],
        SettingsPage::Epg => vec!["guide", "xmltv", "schedule"],
        SettingsPage::Favourites => vec!["favorites", "saved", "lists"],
        SettingsPage::Reminders => vec!["notify", "notification", "alarm"],
        SettingsPage::Following => vec!["rules", "automatic", "teams"],
        SettingsPage::NewEpisodes => vec!["episodes", "alerts", "notifications", "binge"],
        SettingsPage::SpoilerGuard => vec!["spoilers", "scores", "results"],
        SettingsPage::Parental => vec!["pin", "lock", "kids"],
        SettingsPage::Agents => vec!["claude", "mcp", "codex"],
        SettingsPage::Cast => vec!["casting", "airplay", "tv", "apple tv"],
        SettingsPage::Backup => vec!["backup", "export", "restore"],
        SettingsPage::Hidden => vec!["unhide", "categories", "countries"],
        SettingsPage::Orphans => vec!["missing", "reattach"],
        _ => Vec::new(),
    };
    let title = title.to_lowercase();
    for (names, aliases) in [
        (&["start screen"][..], &["open on", "launch", "startup", "home"][..]),
        (&["audio delay", "lip sync"][..], &["lip sync", "audio offset", "out of sync", "sound earlier", "sound later"][..]),
        (&["hardware decoding"][..], &["hwdec", "gpu"][..]),
        (&["subtitle"][..], &["captions", "cc"][..]),
        (&["sidebar"][..], &["menu"][..]),
        (&["theme", "density", "font", "scale"][..], &["look", "size", "appearance"][..]),
        (&["buffer", "demuxer"][..], &["cache", "memory", "stutter"][..]),
        (&["skip"][..], &["seek", "jump"][..]),
        (&["zapping"][..], &["channel switching"][..]),
        (&["resume", "watched"][..], &["continue", "progress"][..]),
        (&["language"][..], &["preferred", "track"][..]),
        (&["pop-out"][..], &["pip", "picture in picture", "window"][..]),
        (&["preview"][..], &["autoplay"][..]),
        (&["clock"][..], &["time", "24 hour"][..]),
        (&["inhibit", "awake"][..], &["sleep", "screensaver"][..]),
        (&["same title", "copy plays"][..], &["duplicates", "combine"][..]),
        (&["update", "install", "check now", "restart now"][..], &["upgrade", "version"][..]),
        (&["log"][..], &["debug", "diagnostics"][..]),
    ] {
        if names.iter().any(|name| title.contains(name)) {
            words.extend_from_slice(aliases);
        }
    }
    words
}

impl SettingsScreen {
    pub(crate) fn indexed_rows(&self, ctx: &UiCtx<'_>) -> Vec<IndexedRow> {
        let mut out = Vec::new();
        for (page, _, page_name) in pages() {
            let rows = self.page_rows(ctx, page);
            let mut section = page_name.to_owned();
            let mut intro = String::new();
            for (position, row) in rows.iter().enumerate() {
                match row {
                    Row::Section(name) => {
                        section.clone_from(name);
                        intro.clear();
                        continue;
                    }
                    Row::Note(note, _) => {
                        intro.push(' ');
                        intro.push_str(note);
                        continue;
                    }
                    _ => {}
                }
                if let Row::Item { item, .. } = row {
                    let visible = match item {
                        Item::Reminder(id) => self.reminders.iter().find(|r| r.id == *id).is_some_and(|r| searchable_item(ctx, &r.channel)),
                        Item::HiddenChannel(id) => !kit::channel_locked(ctx, *id),
                        Item::HiddenVod(item) | Item::Orphan(item) | Item::TitleLink(item, _) => searchable_item(ctx, item),
                        Item::HiddenGroup(id) | Item::Explicit(id) => kit::unlocked(ctx) || !all_groups(ctx).any(|g| g.id == *id && g.locked),
                        _ => true,
                    };
                    if !visible {
                        intro.clear();
                        continue;
                    }
                }
                let title = row.title();
                let key = match row {
                    Row::Item { item, .. } => SettingKey::Item(item.clone()),
                    Row::Act { act, .. } => SettingKey::Action(*act),
                    Row::Local { field, .. } => SettingKey::Local(*field),
                    Row::LipSync { output, reset } => SettingKey::AudioOutput(output.name.clone(), *reset),
                    _ => SettingKey::Control(title.to_owned()),
                };
                let target = SettingTarget { page, title: title.to_owned(), key };
                let mut description = format!("{page_name} {section} {intro}");
                if let Row::Item { detail: Some(detail), .. } = row {
                    description.push(' ');
                    description.push_str(detail);
                }
                for following in &rows[position + 1..] {
                    if let Row::Note(note, _) = following {
                        description.push(' ');
                        description.push_str(note);
                    } else {
                        break;
                    }
                }
                let document = SettingDocument::new(title, &description, &words(page, title));
                out.push(IndexedRow { target, document, row: row.clone(), position });
                intro.clear();
            }
        }
        out
    }

    pub(super) fn search_rows(&self, ctx: &UiCtx<'_>) -> Vec<(Option<SettingTarget>, Row)> {
        let mut hits: Vec<_> =
            self.indexed_rows(ctx).into_iter().filter_map(|row| row.document.score(&self.filter).map(|score| (score, row))).collect();
        // Rank the page groups by their best hit, then the hits within each page.
        let mut groups = Vec::new();
        hits.sort_by_key(|(score, _)| *score);
        for (_, row) in &hits {
            if !groups.contains(&row.target.page) {
                groups.push(row.target.page);
            }
        }
        let mut out = Vec::new();
        for page in groups {
            for (_, row) in hits.iter().filter(|(_, row)| row.target.page == page) {
                if out.last().is_none_or(|(target, _): &(Option<SettingTarget>, Row)| target.as_ref().is_none_or(|t| t.page != page)) {
                    out.push((None, Row::Section(row.target.page_name().to_owned())));
                }
                out.push((Some(row.target.clone()), row.row.clone()));
            }
        }
        out
    }

    pub(crate) fn load_search_pages(&mut self, ctx: &UiCtx<'_>) {
        self.search_all = true;
        let page = self.page;
        // Load stored lists off the UI thread, just as visiting each page does. Do not start
        // discovery or agent setup merely because the search overlay opened.
        for p in [SettingsPage::Playlists, SettingsPage::Epg, SettingsPage::Reminders, SettingsPage::Hidden, SettingsPage::Orphans] {
            self.page = p;
            self.reload(ctx);
        }
        if crate::follow::ENABLED {
            following::enter(self, ctx);
        }
        if crate::new_episodes::ENABLED {
            new_episodes::enter(self, ctx);
        }
        if crate::spoiler::ENABLED {
            spoiler::enter(self, ctx);
        }
        self.page = page;
    }

    pub(crate) fn poll_search_pages(&mut self, ctx: &mut UiCtx<'_>) -> bool {
        self.poll_loaders(ctx);
        self.sources_loader.is_none()
            && self.epg_loader.is_none()
            && self.reminders_loader.is_none()
            && self.hidden_loader.is_none()
            && self.links_loader.is_none()
            && self.orphans_loader.is_none()
            && following::search_ready(self)
            && new_episodes::search_ready(self)
    }

    pub(super) fn set_filter(&mut self, ctx: &UiCtx<'_>, query: String) {
        if query == self.filter || (self.filter.is_empty() && query.trim().is_empty()) {
            return;
        }
        if query.is_empty() {
            self.clear_filter();
            return;
        }
        if self.filter_return.is_none() {
            self.filter_return = Some((self.page, self.pane, self.row));
            self.load_search_pages(ctx);
        }
        self.filter = query;
        self.pane = Pane::Content;
        self.row = 0;
        self.scroll_to_row = true;
    }

    pub(super) fn clear_filter(&mut self) {
        self.filter.clear();
        self.search_all = false;
        self.search_focused = false;
        self.release_search_focus = true;
        if let Some((page, pane, row)) = self.filter_return.take() {
            self.page = page;
            self.pane = pane;
            self.row = row;
        }
        self.scroll_to_row = true;
    }

    pub(super) fn focus_target(&mut self, ctx: &UiCtx<'_>, target: &SettingTarget) -> bool {
        let Some(hit) = self.indexed_rows(ctx).into_iter().find(|r| r.target.page == target.page && r.target.key == target.key) else { return false };
        self.row = hit.position;
        self.pane = Pane::Content;
        self.scroll_to_row = true;
        true
    }

    pub(super) fn jump_to(&mut self, ctx: &mut UiCtx<'_>, target: &SettingTarget) -> bool {
        if target.page != self.page && !self.may_leave_editor(ctx) {
            return false;
        }
        self.filter.clear();
        self.search_all = false;
        self.filter_return = None;
        self.search_focused = false;
        self.release_search_focus = true;
        self.set_page(ctx, target.page);
        self.focus_target(ctx, target)
    }

    /// Parental prompts resolve against their page, including the offer to set a PIN before
    /// showing explicit content. Other controls can keep the filtered results in place.
    pub(super) fn adjust_hit(&mut self, ctx: &mut UiCtx<'_>, target: &SettingTarget, row: &Row, dir: i32) {
        let parental_prompt = target.page == SettingsPage::Parental
            && (self.parental_locked(ctx) || matches!(row, Row::Toggle(t) if std::ptr::eq(*t, &PARENTAL_SHOW_EXPLICIT)));
        if parental_prompt {
            if self.jump_to(ctx, target) {
                self.adjust(ctx, row, dir);
            }
        } else {
            let page = self.page;
            self.page = target.page;
            self.adjust(ctx, row, dir);
            self.page = page;
        }
    }

    pub(super) fn filter_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool {
        if self.pane == Pane::Pages
            && matches!(action, Action::Activate | Action::Right | Action::Increase | Action::Decrease | Action::ContextMenu | Action::Remove)
        {
            return false;
        }
        let mut query = self.filter.clone();
        match action {
            Action::FilterInput(' ') if query.is_empty() && !self.search_focused => {
                if self.pane == Pane::Pages {
                    self.pane = Pane::Content;
                } else {
                    self.submit = true;
                    self.submit_input = ctx.config_input;
                }
                return true;
            }
            Action::FilterInput(c) => query.push(c),
            Action::FilterBackspace => {
                query.pop();
            }
            Action::FilterDeleteWord => {
                let trimmed = query.trim_end();
                query.truncate(trimmed.rfind(char::is_whitespace).map_or(0, |i| i + trimmed[i..].chars().next().unwrap().len_utf8()));
            }
            Action::FilterClear | Action::Back if !query.is_empty() || self.search_focused => {
                self.clear_filter();
                return true;
            }
            Action::Activate if !query.is_empty() => {
                let rows = self.search_rows(ctx);
                let plain: Vec<_> = rows.iter().map(|(_, r)| r.clone()).collect();
                self.clamp_row(&plain);
                if let Some((Some(target), _)) = rows.get(self.row) {
                    self.jump_to(ctx, target);
                }
                return true;
            }
            Action::ContextMenu | Action::Remove if !query.is_empty() => {
                let rows = self.search_rows(ctx);
                let plain: Vec<_> = rows.iter().map(|(_, r)| r.clone()).collect();
                self.clamp_row(&plain);
                if let Some((Some(target), Row::Item { item, .. })) = rows.get(self.row)
                    && self.jump_to(ctx, target)
                {
                    self.item_menu(ctx, item.clone());
                }
                return true;
            }
            Action::Right | Action::Increase | Action::Decrease if !query.is_empty() => {
                let rows = self.search_rows(ctx);
                let plain: Vec<_> = rows.iter().map(|(_, r)| r.clone()).collect();
                self.clamp_row(&plain);
                if let Some((Some(target), row)) = rows.get(self.row) {
                    self.adjust_hit(ctx, target, row, if action == Action::Decrease { -1 } else { 1 });
                }
                return true;
            }
            _ => return false,
        }
        self.set_filter(ctx, query);
        true
    }
}
