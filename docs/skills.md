# Skills

JuCode loads skills from these sources:

1. installed user skills under `~/.jucode/skills`;
2. project skills under `<project>/.jucode/skills`, only after the project is trusted;
3. the JuCode marketplace returned by `/v1/skills/marketplace`.

Each skill is a directory containing `SKILL.md`. Frontmatter `name` and `description` fields
are used for discovery. A skill named `Code Review` is available as `/code-review`; text after
the slash command is passed to the skill as the user request. `/pin <skill>` keeps a skill's
instructions in the current session context.

## Lifecycle commands

```text
/skills list
/skills install <id>
/skills update <id>
/skills uninstall <id>
/skills enable <id>
/skills disable <id>
/skills sync
```

Install and update target the user skill directory. Disable keeps the files installed but
removes the skill from model context and slash-command discovery. Sync installs or updates the
marketplace's configured default skills. Project skills are source-controlled local resources,
so lifecycle commands do not modify them.

Marketplace packages must include `package_sha256`. Downloads are capped at 20 MiB, extracted
content at 100 MiB and 4,096 files. Zip and tar.gz packages reject absolute paths, parent
traversal, links, and special files. File permissions are preserved. Extraction happens in a
sibling temporary directory and the completed skill is renamed into place, so a failed update
leaves the previous install intact.
