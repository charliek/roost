# Discovery notes

Working notes, not product commitments. These files live outside
`docs/` so they are not built into the public site — Zensical has
no equivalent of MkDocs `exclude_docs`.

| Note | What it is |
|---|---|
| [host-sessions.md](host-sessions.md) | Opt-in persist + SSH: `roost-session`, iced as a smart libghostty client, Superlogical-shaped attach. Rationale document; the roadmap + architecture notes below supersede it where they differ. |
| [host-sessions-roadmap.md](host-sessions-roadmap.md) | The ordered HS-0…HS-5 milestones and pinned decisions (D1–D11). HS-0 through HS-2 have shipped; HS-3 (SSH transport) is next. |
| [host-sessions-architecture.md](host-sessions-architecture.md) | The normative protocol/architecture design the HS plans implement — data plane, leases, effects, SSH topology. |
| [agent-watching.md](agent-watching.md) | Expanding agent coverage (hooks + screen/process fallback) without blocking host sessions. |
| [swift-retirement.md](swift-retirement.md) | Retiring the Swift `Roost.app`: iced takes over the Mac identity, the Mac feature gaps to close (with what plan 073, the first gap release, shipped or deferred), the removal inventory, the `roost-legacy-swift` extraction, issue triage, and phases: Swift frozen, gap releases, then the v0.1.0 cutover. |
| [text-rendering.md](text-rendering.md) | Tuning iced's terminal text, especially on macOS: how it renders today, a side-by-side with the Swift app, lessons from Ghostty and cosmic-term, and tiered options. Marks what plan 073 shipped (all of Tier 0, plus cursor, selection and macOS cell metrics) and its findings. |
| [sessions-everywhere.md](sessions-everywhere.md) | Making sessions the normal backend on every machine so work doesn't depend on where you sit: what already works for several clients, the gaps a real Mac→Pop!_OS setup hit (finding the session when `XDG_RUNTIME_DIR` differs, window size with several clients, reconnect, daemon lifecycle, where session tabs are worse than in-process), a possible order, and what retiring in-process local tabs would involve. |
| [mobile-client.md](mobile-client.md) | A Flutter mobile client (Android first; tablets, foldables and phones) for host sessions: what an Android spike proved end to end (libghostty-vt, `roost-vt`, `roost-ipc` and russh, reaching a real `roost-session`), the crate layout (`roost-client` extracted from `roost-iced`, an FRB crate, `mobile/`), the SSH transport choice, sizing with several clients, mobile UX in place of hotkeys, and an MVP order. |
| [protocol-7-release.md](protocol-7-release.md) | **Decided (2026-10-28):** the next release and the source of truth for its milestone plans. M1: Ghostty main with Zig 0.17, OSC 7501 program status, per-tab memory. M2: the Linux session socket under HOME (G1). M3: tab size ownership (G2). M4: tab environment by opener (G9). M5: the first `roost-client` extraction. One session-protocol bump, 6 → 7. |
| [chrome-themes.md](chrome-themes.md) | Chrome themes, user color overrides, per-region translucency, and following system light/dark: what plan 073's `ChromePalette` seam allows, what iced 0.14 and winit already support on each OS, and the open questions for a post-v0.1.0 planning session (#584, #164). |

Update the notes when a later pass changes a recommendation. Implementation plans and `docs/development/vision.md` decision-log edits belong in the PR that actually builds the feature.
