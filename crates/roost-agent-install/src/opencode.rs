//! opencode — the forwarding plugin, written into
//! `~/.config/opencode/plugins/`.
//!
//! opencode has no command hooks, so there is nothing to merge into: the
//! integration is a small JavaScript plugin that subscribes to the bus
//! and forwards a whitelist of events to `agent-hook opencode`. The
//! plugin itself carries no policy — `roost_agent::opencode` is the
//! single source of truth for what an event means — so this writer is
//! just "put the shipped asset here, with a header saying who wrote it".
//!
//! That header **is** the ownership rule for this file. A
//! `roost-agent-state.js` without it belongs to somebody else and is
//! never overwritten and never deleted.

use std::path::PathBuf;

use roost_agent::Agent;

use crate::command::INTEGRATION_VERSION;
use crate::error::{InstallError, SkipReason};
use crate::home::Home;
use crate::plan::{FileEdit, InstallPlan, Intent};
use crate::write::{self, PRIVATE_MODE};

const AGENT: Agent = Agent::Opencode;

/// The header line of any plugin file Roost wrote, up to the version
/// number that ends it.
const MARKER: &str = "// managed by roost — agent hooks, integration version ";

pub fn plugin_path(home: &Home) -> PathBuf {
    home.agent_dir(AGENT)
        .join("plugins")
        .join(roost_agent::opencode::PLUGIN_FILE_NAME)
}

fn desired() -> Vec<u8> {
    format!(
        "{MARKER}{INTEGRATION_VERSION}\n\
         // Written by `roostctl agent install opencode`; removed by\n\
         // `roostctl agent uninstall opencode`. Local edits are overwritten.\n\
         {plugin}",
        plugin = roost_agent::opencode::PLUGIN_SOURCE
    )
    .into_bytes()
}

/// The claim is the **whole first line**, not a prefix of it.
///
/// A `starts_with` over the words alone says yes to
/// `// managed by roostling` and to `// managed by roost -- my own
/// file`, and this answer is what lets Roost overwrite and then delete
/// the file. Any integration version is accepted, because an older one
/// is still ours to refresh.
fn is_ours(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    let first = text.lines().next().unwrap_or_default();
    first
        .strip_prefix(MARKER)
        .is_some_and(|version| version.parse::<u32>().is_ok())
}

pub fn plan_install(home: &Home) -> Result<InstallPlan, InstallError> {
    let path = plugin_path(home);
    let image = write::read_image(&path, PRIVATE_MODE)?;
    if let Some(existing) = image.bytes.as_deref() {
        if !is_ours(existing) {
            return Ok(InstallPlan::skip(
                AGENT,
                Intent::Install,
                SkipReason::ForeignFile { path },
            ));
        }
    }

    let want = desired();
    let edits = if image.bytes.as_deref() == Some(want.as_slice()) {
        Vec::new()
    } else {
        vec![FileEdit {
            image,
            after: Some(want),
        }]
    };

    Ok(InstallPlan {
        agent: AGENT,
        intent: Intent::Install,
        edits,
        skipped: None,
        warnings: Vec::new(),
        files: vec![path],
    })
}

pub fn plan_uninstall(home: &Home) -> Result<InstallPlan, InstallError> {
    let path = plugin_path(home);
    let image = write::read_image(&path, PRIVATE_MODE)?;
    let Some(existing) = image.bytes.as_deref() else {
        return Ok(InstallPlan {
            agent: AGENT,
            intent: Intent::Uninstall,
            edits: Vec::new(),
            skipped: None,
            warnings: Vec::new(),
            files: Vec::new(),
        });
    };
    if !is_ours(existing) {
        return Ok(InstallPlan::skip(
            AGENT,
            Intent::Uninstall,
            SkipReason::ForeignFile { path },
        ));
    }

    Ok(InstallPlan {
        agent: AGENT,
        intent: Intent::Uninstall,
        edits: vec![FileEdit { image, after: None }],
        skipped: None,
        warnings: Vec::new(),
        files: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_gets_written_is_the_shipped_plugin_under_a_marker() {
        let bytes = desired();
        assert!(is_ours(&bytes));
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains(&format!("integration version {INTEGRATION_VERSION}")));
        assert!(text.ends_with(roost_agent::opencode::PLUGIN_SOURCE));
    }

    /// A prefix test says yes to files that merely start the same way.
    /// The header Roost writes is a whole line, and only that line is
    /// the claim.
    #[test]
    fn a_lookalike_header_is_not_the_marker() {
        assert!(is_ours(&desired()));
        for foreign in [
            "// managed by roostling\nexport const Plugin = () => ({});\n",
            "// managed by roost -- my own file\n",
            "// managed by roost\n",
            "// managed by roost — agent hooks, integration version two\n",
            "  // managed by roost — agent hooks, integration version 1\n",
        ] {
            assert!(
                !is_ours(foreign.as_bytes()),
                "{foreign:?} was claimed as Roost's"
            );
        }
    }

    /// A plugin of the same name that Roost did not write is somebody
    /// else's file. Neither install nor uninstall may touch it.
    #[test]
    fn a_plugin_without_the_marker_is_never_touched() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::rooted(dir.path());
        let path = plugin_path(&home);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"// somebody else's plugin\n").unwrap();

        for plan in [plan_install(&home).unwrap(), plan_uninstall(&home).unwrap()] {
            assert!(plan.edits.is_empty());
            assert!(matches!(plan.skipped, Some(SkipReason::ForeignFile { .. })));
        }
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"// somebody else's plugin\n"
        );
    }
}
