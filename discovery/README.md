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

Update the notes when a later pass changes a recommendation. Implementation plans and `docs/development/vision.md` decision-log edits belong in the PR that actually builds the feature.
