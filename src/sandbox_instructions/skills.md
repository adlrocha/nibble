## Skills & Lessons

Skills are stored on the **host** at `~/.claude/skills/` and bind-mounted into every sandbox at `/home/node/.claude/skills/`. This means:

- Skill updates made inside a sandbox are immediately visible on the host and in all other sandboxes — they share the same directory.
- To persist a skill or lessons-learned update, edit the skill file directly (e.g. `~/.claude/skills/nibble-memory/SKILL.md`). No restart or re-injection needed.
- Edits to nibble-shipped skills in that directory are overwritten by the next install. For lasting skill changes, edit the nibble source checkout and submit them through its PR flow. Record lessons with `nibble memory lesson-add` instead of editing an installed skill.
- The host `install.sh` re-installs skills from the nibble repo to `~/.claude/skills/` whenever you update them in source. Run it after editing skills in the repo.
