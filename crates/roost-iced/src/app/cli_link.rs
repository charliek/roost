//! The `roostctl` link for terminals outside Roost (#261), in the window:
//! `app.cli_link`, the macOS palette row and App-menu item, and what each
//! outcome shows — a toast for a plain success, a card when the user has
//! something to do. The link itself is `roost_ipc::cli_link`'s.

use std::path::PathBuf;

use roost_ipc::cli_link::{self, Action, Context};
use roost_ipc::messages::AppCliLinkResult;

use super::{host_dialog, palette, App, Severity, UiTask};

/// One row id per action, so a row does what its label said when the
/// palette opened rather than what the directories say a moment later.
pub(super) const INSTALL_ROW_ID: &str = "install_cli";
pub(super) const REPAIR_ROW_ID: &str = "repair_cli";
pub(super) const UNINSTALL_ROW_ID: &str = "uninstall_cli";

/// What the card's primary button does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CardAction {
    /// Put this text on the clipboard: the `PATH` line, or the command
    /// that does by hand what Roost would not.
    Copy(String),
    Repair,
}

/// A card for the link: something the user has to read or do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Card {
    pub(super) title: String,
    pub(super) body: String,
    pub(super) dismiss: &'static str,
    pub(super) primary: Option<(&'static str, CardAction)>,
}

impl Card {
    pub(super) fn buttons(&self) -> Vec<String> {
        std::iter::once(self.dismiss.to_string())
            .chain(self.primary.as_ref().map(|(label, _)| label.to_string()))
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Feedback {
    Silent,
    Toast(String),
    Card(Card),
}

/// What the window shows for `result`.
pub(super) fn feedback(result: &AppCliLinkResult) -> Feedback {
    match result.outcome.as_str() {
        "status" => Feedback::Silent,
        "refused" => {
            let mut body = format!("{}.", result.message);
            if let Some(command) = &result.command {
                body.push_str(&format!("\n\nTo do it by hand, run:\n{command}"));
            }
            Feedback::Card(Card {
                title: "Couldn't Change the 'roostctl' Command".into(),
                body,
                dismiss: "Close",
                primary: result
                    .command
                    .clone()
                    .map(|command| ("Copy Command", CardAction::Copy(command))),
            })
        }
        "created" | "repaired" | "already_installed" => match &result.path_line {
            Some(line) => Feedback::Card(Card {
                title: "'roostctl' Command Installed".into(),
                body: format!(
                    "{}.\n\nThat directory may not be on your shell's PATH. If a new terminal \
                     can't find roostctl, add this line to ~/.zprofile (zsh) or \
                     ~/.bash_profile (bash):\n{line}",
                    result.message
                ),
                dismiss: "Close",
                primary: Some(("Copy Line", CardAction::Copy(line.clone()))),
            }),
            None => Feedback::Toast(result.message.clone()),
        },
        _ => Feedback::Toast(result.message.clone()),
    }
}

/// The confirmation the Repair… row opens: what the link points at now,
/// and what Repair points it at.
pub(super) fn repair_card(result: &AppCliLinkResult) -> Card {
    Card {
        title: "Repair the 'roostctl' Command".into(),
        body: format!(
            "{} points at {}, which is not this Roost's roostctl. Repair points it at {}.",
            result.link.as_deref().unwrap_or("The link"),
            result.target.as_deref().unwrap_or("another app"),
            result.source.as_deref().unwrap_or("this Roost's roostctl"),
        ),
        dismiss: "Cancel",
        primary: Some(("Repair", CardAction::Repair)),
    }
}

/// The row for `status`, macOS only and only for a Roost whose
/// `roostctl` sits in its own app bundle: anywhere else `roostctl` is
/// installed where it is, or not at all.
pub(super) fn row(status: &AppCliLinkResult, own_bundle: bool) -> Option<palette::PaletteItem> {
    if !(cfg!(target_os = "macos") && own_bundle) {
        return None;
    }
    let id = match status.state.as_str() {
        "installed" => UNINSTALL_ROW_ID,
        "broken" => REPAIR_ROW_ID,
        _ => INSTALL_ROW_ID,
    };
    Some(palette::PaletteItem::new(id, status.label.clone()))
}

impl App {
    /// Run `action` against this Roost's own `roostctl` and show what came
    /// of it. Every caller — `app.cli_link`, the palette row, the menu
    /// item, the card — comes through here.
    pub(super) fn cli_link(&mut self, action: Action) -> AppCliLinkResult {
        let result = cli_link_act(action);
        if action != Action::Status {
            tracing::info!(
                action = ?action,
                outcome = %result.outcome,
                state = %result.state,
                link = ?result.link,
                "cli link"
            );
        }
        match feedback(&result) {
            Feedback::Silent => {}
            Feedback::Toast(text) => self.set_status_as(text, Severity::Info),
            Feedback::Card(card) => self.open_host_dialog(host_dialog::HostDialog::CliLink(card)),
        }
        self.sync_cli_link_menu(&result);
        result
    }

    /// Whether this Roost's `roostctl` lies in its own app bundle.
    pub(super) fn cli_link_bundled() -> bool {
        roost_engine::process::roostctl().is_some_and(|found| found.bundle_bin.is_some())
    }

    /// The palette row, read fresh: the directories are the truth, and
    /// another terminal may have changed them since.
    pub(super) fn cli_link_row() -> Option<palette::PaletteItem> {
        if !cfg!(target_os = "macos") {
            return None;
        }
        row(&cli_link_act(Action::Status), Self::cli_link_bundled())
    }

    /// A palette row or the menu item: Repair confirms first; Install and
    /// Uninstall act.
    pub(super) fn cli_link_requested(&mut self, id: &str) {
        match id {
            REPAIR_ROW_ID => {
                let status = cli_link_act(Action::Status);
                if status.state == "broken" {
                    self.open_host_dialog(host_dialog::HostDialog::CliLink(repair_card(&status)));
                } else {
                    self.cli_link(Action::Install);
                }
            }
            UNINSTALL_ROW_ID => {
                self.cli_link(Action::Uninstall);
            }
            _ => {
                self.cli_link(Action::Install);
            }
        }
    }

    /// The menu item does what its title says now.
    #[cfg(target_os = "macos")]
    pub(super) fn cli_link_menu_activated(&mut self) {
        let status = cli_link_act(Action::Status);
        let id = row(&status, true).map(|item| item.id);
        self.cli_link_requested(id.as_deref().unwrap_or(INSTALL_ROW_ID));
    }

    /// The card's primary button.
    pub(super) fn cli_link_card_confirmed(&mut self) -> UiTask {
        let Some(host_dialog::HostDialog::CliLink(card)) = self.host_dialog.take() else {
            return UiTask::None;
        };
        match card.primary {
            Some((_, CardAction::Copy(text))) => {
                self.clipboard
                    .enqueue_write(roost_engine::ipc::ClipboardOp::System, text);
                self.set_status_as("Copied to the clipboard", Severity::Info);
                self.clipboard.start_next()
            }
            Some((_, CardAction::Repair)) => {
                self.cli_link(Action::Install);
                UiTask::None
            }
            None => UiTask::None,
        }
    }

    /// The App-menu item's title now.
    #[cfg(target_os = "macos")]
    pub(super) fn cli_link_menu_title(&self) -> String {
        cli_link_act(Action::Status).label
    }

    /// Re-read the link and push the App-menu item's title, when it moved.
    pub(super) fn refresh_cli_link_menu(&mut self) {
        if cfg!(target_os = "macos") {
            let status = cli_link_act(Action::Status);
            self.sync_cli_link_menu(&status);
        }
    }

    /// Only a menu built with the item has one to retitle.
    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
    fn sync_cli_link_menu(&mut self, status: &AppCliLinkResult) {
        #[cfg(target_os = "macos")]
        {
            let Some(shown) = self.menu_cli_link.as_ref() else {
                return;
            };
            if *shown == status.label {
                return;
            }
            let Some(mtm) = super::servicing::seam_on_main("cli link menu sync") else {
                return;
            };
            crate::macos::menu::sync_cli_link_item(mtm, &status.label);
            self.menu_cli_link = Some(status.label.clone());
        }
    }
}

/// [`cli_link::act`] with this process's own `roostctl`, directories,
/// `PATH` and `HOME`.
fn cli_link_act(action: Action) -> AppCliLinkResult {
    let source = roost_engine::process::roostctl().map(|found| found.path.as_path());
    let dirs = cli_link::dirs_from_env();
    let path_var = std::env::var_os("PATH");
    let home = std::env::var_os("HOME").map(PathBuf::from);
    cli_link::act(
        action,
        Context {
            source,
            dirs: &dirs,
            path_var: path_var.as_deref(),
            home: home.as_deref(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(outcome: &str) -> AppCliLinkResult {
        AppCliLinkResult {
            state: "installed".into(),
            label: "Uninstall 'roostctl' Command".into(),
            link: Some("/usr/local/bin/roostctl".into()),
            source: Some("/Applications/Roost.app/Contents/Resources/bin/roostctl".into()),
            bundled: true,
            outcome: outcome.into(),
            message: "Installed 'roostctl': /usr/local/bin/roostctl → …".into(),
            ..AppCliLinkResult::default()
        }
    }

    #[test]
    fn a_plain_success_is_a_toast() {
        for outcome in [
            "created",
            "repaired",
            "already_installed",
            "removed",
            "nothing_to_remove",
        ] {
            assert_eq!(
                feedback(&result(outcome)),
                Feedback::Toast(result(outcome).message),
                "{outcome}"
            );
        }
        assert_eq!(feedback(&result("status")), Feedback::Silent);
    }

    #[test]
    fn a_link_off_path_is_a_card_with_the_line_to_copy() {
        let line = "export PATH=\"$HOME/.local/bin:$PATH\"";
        let off_path = AppCliLinkResult {
            path_line: Some(line.into()),
            ..result("created")
        };
        let Feedback::Card(card) = feedback(&off_path) else {
            panic!("a card");
        };
        assert!(card.body.ends_with(line), "{}", card.body);
        assert!(card.body.contains("~/.zprofile"), "{}", card.body);
        assert_eq!(
            card.primary,
            Some(("Copy Line", CardAction::Copy(line.into())))
        );
        assert_eq!(card.buttons(), vec!["Close", "Copy Line"]);
    }

    #[test]
    fn a_refusal_is_a_card_with_the_command_when_there_is_one() {
        let command =
            "ln -sf '/A/Roost.app/Contents/Resources/bin/roostctl' '/usr/local/bin/roostctl'";
        let refused = AppCliLinkResult {
            message: "/usr/local/bin/roostctl already exists and is not a link Roost made".into(),
            command: Some(command.into()),
            ..result("refused")
        };
        let Feedback::Card(card) = feedback(&refused) else {
            panic!("a card");
        };
        assert!(card.body.ends_with(command), "{}", card.body);
        assert_eq!(
            card.primary,
            Some(("Copy Command", CardAction::Copy(command.into())))
        );

        let translocated = AppCliLinkResult {
            message: "Roost is running from a temporary copy".into(),
            ..result("refused")
        };
        let Feedback::Card(card) = feedback(&translocated) else {
            panic!("a card");
        };
        assert_eq!(card.primary, None);
        assert_eq!(card.buttons(), vec!["Close"]);
    }

    #[test]
    fn the_row_follows_link_presence_and_needs_a_bundle() {
        let row_for = |state: &str, label: &str, bundled: bool| {
            row(
                &AppCliLinkResult {
                    state: state.into(),
                    label: label.into(),
                    ..result("status")
                },
                bundled,
            )
            .map(|item| (item.id, item.title))
        };
        if cfg!(target_os = "macos") {
            assert_eq!(
                row_for("absent", "Install 'roostctl' Command", true),
                Some((INSTALL_ROW_ID.into(), "Install 'roostctl' Command".into()))
            );
            assert_eq!(
                row_for("broken", "Repair 'roostctl' Command…", true),
                Some((REPAIR_ROW_ID.into(), "Repair 'roostctl' Command…".into()))
            );
            assert_eq!(
                row_for("installed", "Uninstall 'roostctl' Command", true),
                Some((
                    UNINSTALL_ROW_ID.into(),
                    "Uninstall 'roostctl' Command".into()
                ))
            );
        }
        assert_eq!(row_for("absent", "Install 'roostctl' Command", false), None);
    }

    /// Linux has no row: `/usr/bin/roostctl` is on PATH already.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn there_is_no_row_off_macos() {
        assert_eq!(
            row(
                &AppCliLinkResult {
                    state: "absent".into(),
                    ..result("status")
                },
                true
            ),
            None
        );
    }

    #[test]
    fn the_repair_card_names_both_ends() {
        let broken = AppCliLinkResult {
            state: "broken".into(),
            target: Some("/Old/Roost.app/Contents/Resources/bin/roostctl".into()),
            ..result("status")
        };
        let card = repair_card(&broken);
        assert!(
            card.body.contains("/usr/local/bin/roostctl"),
            "{}",
            card.body
        );
        assert!(card.body.contains("/Old/Roost.app"), "{}", card.body);
        assert!(
            card.body.contains("/Applications/Roost.app"),
            "{}",
            card.body
        );
        assert_eq!(card.buttons(), vec!["Cancel", "Repair"]);
    }
}
