# Skills

JuCode loads skills from these sources:

1. installed user skills under `~/.jucode/skills`;
2. project skills under `<project>/.jucode/skills`, only after the project is trusted;
3. the JuCode marketplace returned by `/v1/skills/marketplace`;
4. one optional extra GitHub source configured in `~/.jucode/config.json`.

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

## Extra GitHub source

Set `extra_skills_source` to the built-in name `anthropic` or to an HTTPS GitHub repository:

```json
{
  "extra_skills_source": "anthropic"
}
```

The built-in source points to <https://github.com/anthropics/skills>. Its small vendored index is
pinned to a reviewed Git commit so listing works without a live GitHub directory request. A custom
repository URL is expected to contain skills at `skills/<slug>/SKILL.md`; JuCode reads its directory
through the public GitHub API. `/skills list` groups available skills by source, and
`/skills install <slug>` or `/skills update <slug>` downloads the selected `SKILL.md`. Source IDs
and paths are validated before any write.

Anthropic's `docx`, `pdf`, `pptx`, and `xlsx` skills are source-available rather than Apache-2.0.
JuCode lists them as not offered and will not install them from the built-in source.

To refresh the built-in index, obtain the current `main` commit with
`git ls-remote https://github.com/anthropics/skills refs/heads/main`, review the upstream
`skills/` directory and each skill's license, then update the revision and reviewed entries in
`crates/agent-core/src/anthropic-skills-index.json`. Keep source-available entries under
`excluded`, and run the agent-core tests after editing.
