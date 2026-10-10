# AI

[日本語](ja/ai.md)

For the AI features that ForgeKairos shares with Zed (the agent panel, inline assistant,
providers), see [Zed's AI documentation](https://zed.dev/docs/ai/overview). This page
describes what ForgeKairos changes and adds. For diagrams of how AI and network access are
restricted, see [AI and network restrictions at a glance](overview.md).

## Off by default

Every AI feature is off until you turn it on. Use **Enable Agent** at the top of
Settings → AI (the setting `agent.enabled`, default `false`). The change applies without a
restart, and you can turn it off again the same way.

Do not use `disable_ai` to turn AI off: it is a one-way switch.

- No default model is set. When you enable the agent, the first authenticated provider is
  used, so a running local Ollama or LM Studio is picked up without configuration.
- Edit prediction is off (`edit_predictions.provider` is `"none"`).
- The agent panel does not open on startup (`agent.starts_open` is `false`).

## Local LLMs

Ollama (`localhost:11434`), LM Studio (`localhost:1234`) and llama.cpp (`localhost:8080`)
run on loopback, which the network allowlist always allows. They need no consent and no
allowlist entry. OpenAI-compatible and Anthropic-compatible servers can be added as custom
providers.

A provider you added can be edited or removed later with the pencil and trash buttons on
its header in Settings → AI → LLM Providers.

- Editing leaves the API key field empty; an empty field keeps the saved key. Changing the
  API URL asks for the key again, so a saved key is never sent to another URL, and removes
  the key saved for the old URL from the system keychain. Turning on **Connect without an
  API key** removes the saved key too. Neither removes a key that another provider with the
  same URL uses. A new host asks for consent again.
- Renaming the provider moves the agent settings that name it, and its consent, to the new
  name. Saved threads keep the old name.
- Removing asks first and lists what goes: the settings entry, the saved API key (kept when
  another provider uses the same URL), the consent and its allowlist hosts, and the model
  choices that name the provider. **Remove Provider** in the provider's own view does the
  same.
- Providers defined in a release channel, OS or profile override are shown as not editable
  there.

## Cloud providers

Before an external AI provider can be used, ForgeKairos shows a warning that your input and
conversations are sent to the provider and may be stored or used for training. You enable
the provider by ticking the confirmation checkbox. Until then, no request is sent to it.

- The warning lists the hosts the provider needs, with an **Add to Allowlist** button (see
  [Outbound network control](network.md)).
- Accepted providers are stored in `acknowledged_external_ai_providers`. Removing an entry
  brings the warning back. Withdrawing consent offers to remove the hosts that no other
  accepted provider needs.
- Project settings cannot accept the warning for you.
- Copilot and Zed's hosted edit prediction need the same consent.

## Claude subscriptions

The built-in Anthropic provider uses an API key only. To use a Claude Pro or Max
subscription, use Claude Code:

- **Launch Claude Code** opens the official, unmodified Claude Code CLI. With the agent
  enabled it opens as a tab in the agent panel; otherwise in the terminal panel. Sign-in is
  done in the CLI by Anthropic's own flow; ForgeKairos does not handle the URLs or tokens.
- The Claude agent over ACP accepts subscription sign-in with its own `/login`. An API key
  also works and takes precedence when both are set. To hide the subscription sign-in, set
  `agent_servers.<id>.allow_subscription_sign_in` to `false`.
- The built-in agent and the permission judge (below) never use a subscription.

## The IDE server for Claude Code

With `ide_server.enabled` (**Enable IDE Server**, off by default), ForgeKairos runs a
loopback server on `127.0.0.1` that the Claude Code CLI connects to. It makes no outbound
connections. While it runs, it writes `~/.claude/ide/<port>.lock` for each local project.

Through it, the CLI can:

- open diffs in a ForgeKairos tab with Accept and Reject (the CLI does the write)
- read diagnostics, the open editors, the current selection, and the workspace folders
- open and save files
- read the tail of the last failed task (`ide_server.share_task_output`)

With `ide_server.assist_tools` (**Give Agents IDE Tools**, on by default), agents also get an
MCP server named `zed` with these tools: `highlightCode`, `annotateCode`, `startCodeTour`,
`clearAnnotations`, `getHover`, `getDocumentSymbols`, `searchSymbols`, `findDefinition`,
`findReferences`, `getCallHierarchy` and `getTypeLayout`. Annotations are not saved and are
cleared with Esc. Symbol search uses no model and no embeddings.

The built-in agent has the matching tools `hover`, `search_symbols`, `call_hierarchy`,
`type_layout`, `highlight_code`, `annotate_code`, `code_tour` and `clear_annotations`.

## The agent sandbox

Commands an agent runs always run in a sandbox. The agent cannot ask to run outside it.
If the sandbox cannot be created, you can retry or cancel.

- Windows uses a ForgeKairos helper on WSL; Linux uses bubblewrap; macOS uses Seatbelt.
- Credentials such as `~/.ssh`, `.npmrc`, cloud CLI configuration, browser profiles and
  ForgeKairos's own data are not readable from the sandbox.
- External ACP agents run inside the sandbox too (on Windows, in WSL).
- Git operations that write repository metadata (such as commits) are left to you.
- The sandbox's network is limited by the network allowlist.
- `agent.sandbox_permissions.allow_unsandboxed` (default `false`) is the only switch that
  turns the sandbox off. While it is off, every command asks for confirmation.

### Running outside the sandbox

You can start Claude Code or an ACP agent outside the sandbox yourself:

- **Launch Claude Code Outside the Sandbox…** in the agent panel's **+** menu
- the buttons in launch failure notifications and ACP failure cards
- relaunching from the panel's **⋯** menu
- `agent.claude_code_run_outside_sandbox` and `agent_servers.<id>.run_outside_sandbox`
  (both default `false`, read only from the user settings file)

Outside the sandbox, a banner is always shown and the tab name says so (for example
"Claude Code (Outside the Sandbox)"). Your usual `~/.claude` sign-in is used. Consent for an ACP thread is kept per
thread; revoke it on the AI page (**Consent to Launch Outside the Sandbox**).

## Tool permissions

`agent.tool_permissions.default` takes `"confirm"` (default), `"allow"`, `"deny"` or
`"auto"`. The agent panel's menu offers **Confirm Actions**, **AI Judge (Auto-Run If
Safe)** and **Auto-Approve (In Sandbox)**.

- `"auto"` asks a separate model (`agent.permission_judge_model`, otherwise the fast model,
  then the default model) about each action, with a 10-second limit. Only an answer of
  `SAFE` runs the action without asking. The command and paths are sent to that model's
  provider, so a local model is recommended.
- These are always confirmed, in every mode: built-in security rules and `always_confirm`
  rules, commands outside the sandbox, sandbox escalations, writes to `.zed`, global
  settings, skills or symlinks, and file operations outside the project.

The profiles are **Write** (default), **Ask**, **Edit Only** and **Minimal**. Edit Only reads
and edits files but has no terminal, fetch, delete or MCP tools.

## Pausing project code after agent writes

After an agent writes into a project folder, that folder's own code (formatters, ESLint,
the Python interpreter, rust-analyzer's build scripts) does not run until you review the
changes and click **Resume**. This is on by default
(`agent.pause_worktree_code_after_agent_writes`); only the user settings file can turn it
off.

## MCP servers

Settings → AI → **MCP Servers** shows each server's state, tests connections and adds
servers. Servers you add there go to your user settings. The form has bundled templates
(filesystem, github, playwright, fetch, memory) that show the hosts they need.

MCP servers, language servers and external formatters defined in a project's
`.zed/settings.json` do not start until you approve that exact content; any change asks
again. Approvals are listed under Settings → General → Security, where they can be revoked.
`session.trust_all_worktrees` does not approve them.

## Files hidden from agents

Files matching `private_files` (by default `.env*`, `*.pem`, `*.key`, `*.cert`, `*.crt`,
`secrets.yml`) or `file_scan_exclusions`, files outside the project, and symlinks that point
outside are hidden from the agent's tools and from attachments. For such files, definitions
and references return only the location (path and line), not the text. The check also
catches other spellings of the same path (case, Windows short names, Unicode
normalization).

One response of definitions, references or call hierarchy opens at most 1000 new files and
64 MiB in total.

## Attachments

Large attachments are sent as an outline with a note on how to read more, within the limits
of `agent.context_attachments` (see [Settings reference](settings.md)). Each attachment chip
shows an estimate of its size; right-click it to choose Auto, Full or Summary. Branch diffs
are summarized, and the agent can read the full diff with its `git_diff` tool.

## Model selector

When you have favorite models (`agent.favorite_models`), the other provider groups are
collapsed under a heading with their count, and the group of the model in use stays open.
Search matches group names too. The **Agents** section at the bottom launches Claude Code or
starts a thread with another agent.

## Untrusted folders

In a folder you have not trusted, the agent's profile is reduced to Minimal (no tools). A
notice above the input explains this, and the model is told why it has no tools. Trusting
the folder restores the tools at once.
