//! Downloads settings (DESIGN §43 §2.1, §3). Folder moves wait for workers to close.
use super::*;
use crate::downloads::DownloadCommand;

static NUMBERS: [CfgNumber; 6] = [
    CfgNumber {
        label: "Space cap",
        unit: "GB",
        min: 1.0,
        max: 100000.0,
        step: 10.0,
        get: |c| c.downloads.cap_gb as f64,
        set: |c, v| c.downloads.cap_gb = v as u64,
    },
    CfgNumber {
        label: "Keep free",
        unit: "GB",
        min: 0.0,
        max: 100000.0,
        step: 1.0,
        get: |c| c.downloads.reserve_gb as f64,
        set: |c, v| c.downloads.reserve_gb = v as u64,
    },
    CfgNumber {
        label: "Delete after watching",
        unit: "days (0 = keep)",
        min: 0.0,
        max: 3650.0,
        step: 1.0,
        get: |c| c.downloads.delete_after_watched_days as f64,
        set: |c, v| c.downloads.delete_after_watched_days = v as u32,
    },
    CfgNumber {
        label: "Start recording early",
        unit: "s",
        min: 0.0,
        max: 3600.0,
        step: 30.0,
        get: |c| c.downloads.pad_before_s as f64,
        set: |c, v| c.downloads.pad_before_s = v as u32,
    },
    CfgNumber {
        label: "Stop recording late",
        unit: "s",
        min: 0.0,
        max: 3600.0,
        step: 30.0,
        get: |c| c.downloads.pad_after_s as f64,
        set: |c, v| c.downloads.pad_after_s = v as u32,
    },
    CfgNumber {
        label: "Download speed limit",
        unit: "MB/s (0 = full speed)",
        min: 0.0,
        max: 10000.0,
        step: 0.5,
        get: |c| c.downloads.max_mbps,
        set: |c, v| c.downloads.max_mbps = v,
    },
];
static WINDOW: CfgText = CfgText {
    label: "Only download between",
    placeholder: "Off, or 01:00-07:00",
    get: |c| c.downloads.window.clone(),
    set: |c, v| c.downloads.window = v,
};
pub(super) fn rows(_s: &SettingsScreen, ctx: &UiCtx<'_>, v: &mut Vec<Row>) {
    let snap = crate::ui::downloads::snapshot(ctx);
    v.push(Row::Section("Downloads".into()));
    v.push(Row::Note(
        format!("{:.1} GB used of {:.1} GB · {:.1} GB free", snap.used_bytes as f64 / 1e9, snap.cap_bytes as f64 / 1e9, snap.free_bytes as f64 / 1e9),
        false,
    ));
    v.extend(NUMBERS.iter().map(Row::Number));
    v.push(Row::Text(&WINDOW));
    v.push(Row::Note("Recordings run at their scheduled time, including outside the download window.".into(), false));
    let folder = if ctx.config.downloads.dir.is_empty() { ctx.services.paths.downloads() } else { PathBuf::from(&ctx.config.downloads.dir) };
    v.push(Row::Note(format!("Folder: {}", folder.display()), false));
    v.push(Row::Act { act: Act::Feature(Feature::Downloads, 0), icon: "⤓", label: "Move downloads to another folder…".into(), danger: false });
    v.push(Row::Act {
        act: Act::Feature(Feature::Downloads, 1),
        icon: "⤓",
        label: if snap.paused { "Resume downloads" } else { "Pause downloads" }.into(),
        danger: false,
    });
    v.push(Row::Section("Kept topped up".into()));
    for (i, rule) in snap.rules.iter().enumerate().take(u16::MAX as usize - 2) {
        v.push(Row::Act {
            act: Act::Feature(Feature::Downloads, i as u16 + 2),
            icon: "×",
            label: format!("Delete · {} · keep {} unwatched", rule.title, rule.keep),
            danger: true,
        });
    }
}
pub(super) fn act(_s: &mut SettingsScreen, ctx: &mut UiCtx<'_>, n: u16) {
    let Some(handle) = ctx.services.agent.downloads() else { return };
    if n >= 2 {
        if let Some(rule) = handle.snapshot().rules.get(n as usize - 2) {
            crate::ui::downloads::deleted_rule(ctx, rule.id);
        }
        return;
    }
    if n == 1 {
        handle.send(DownloadCommand::PauseAll(!handle.snapshot().paused));
        return;
    }
    let bus = ctx.services.bus.clone();
    ctx.services.rt.spawn(async move {
        if let Some(folder) = rfd::AsyncFileDialog::new().set_title("Move downloads to an empty folder").pick_folder().await {
            let reply = handle.request(DownloadCommand::MoveFolder(folder.path().to_owned())).await;
            if let Some(error) = reply.error {
                bus.toast(Toast::error(error));
            } else {
                bus.toast(Toast::info("Downloads moved"));
            }
        }
    });
}
