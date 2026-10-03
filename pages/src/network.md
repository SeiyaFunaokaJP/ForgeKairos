# Outbound network control

[日本語](ja/network.md)

ForgeKairos denies outbound traffic started by AI features unless you allow its host. The
list of allowed hosts is the network allowlist.

## What the allowlist controls

The allowlist applies to traffic that AI features start:

- AI model providers and edit prediction providers
- the agent's fetch and web search tools
- MCP servers (HTTP and OAuth)
- external agents (ACP) and the download of their binaries
- the npm that installs agents
- commands an agent runs in the sandbox

It does not restrict the editor's own tools: language servers, formatters, debug adapters,
extensions, node and npm run for those tools, app updates, the remote server, Git, and the
built-in browser (see [Built-in browser](#built-in-browser)).

A request that is not marked as the editor's own traffic is treated as AI traffic.

## Deny by default

A controlled request is refused unless its host matches an entry of the allowlist.
Loopback is always allowed: `127.0.0.0/8`, `::1`, `::ffff:127.0.0.1` and `localhost`
(`*.localhost` is not). Local LLM servers on this machine (Ollama, LM Studio, llama.cpp)
therefore work with an empty list.

Redirects are checked hop by hop against the list. When a redirect goes to another host,
credential headers such as `authorization`, `cookie`, `x-api-key` and `api-key` are
dropped.

Built-in allowances, limited to specific paths, need no entry:

- the extension marketplace
- the ACP agent registry and its icons
- the update check against ForgeKairos's GitHub Releases

## Editing the allowlist

Open Settings → Network → Egress Control → Network Allowlist. There you can add, edit and
remove hosts, reload the file, and open `network_allowlist.json` in the editor (the file is
created if it does not exist). Changes take effect without a restart.

The file is in the config directory, next to `settings.json`:

| OS | Path |
|---|---|
| Windows | `%APPDATA%\ForgeKairos\network_allowlist.json` |
| Linux | `$XDG_CONFIG_HOME/forgekairos/network_allowlist.json` |
| macOS | `~/.config/forgekairos/network_allowlist.json` |

```jsonc
{
  "allowed_hosts": [
    "api.openai.com",
    "*.example.com",
    "[fd00::1]"
  ],
  // optional, true when left out
  "proxy_child_processes": true
}
```

- An entry is an exact host name, `*.domain` (subdomains only, not the domain itself), or an
  IPv4 or IPv6 address.
- Matching ignores case and a trailing dot; international names are compared in Punycode.
- Ports are not matched: allowing a host allows every port.
- Comments and trailing commas are allowed.
- A missing, empty or unreadable file means loopback only. Invalid entries are skipped and
  logged. When the file cannot be parsed, the settings page does not edit it.

## Adding hosts from a consent prompt

- Enabling an external AI provider shows the hosts it needs, with an **Add to Allowlist**
  button. Without them, traffic to that provider stays denied.
- Installing or updating an ACP agent checks the vendor's download hosts and, when one is
  not on the list, asks before adding it. Each update checks again; the consent given at
  installation does not carry over.
- MCP server templates show the hosts they need, with the same button.

## Agents and MCP servers

ACP agents, MCP servers and the npm that installs agents get a loopback proxy through
`HTTP_PROXY` and `HTTPS_PROXY` (`NO_PROXY` is set to loopback). The proxy allows only the
hosts of `allowed_hosts` and loopback; the built-in allowances do not apply. It forwards
through your own proxy setting when one is set.

- Turn it off with `"proxy_child_processes": false`, or with the switch **Apply the Proxy to
  AI Agent Child Processes** in the settings page. A change applies to child processes
  started after it.
- The proxy relies on the program reading the proxy variables. A program that ignores them
  is not restrained.
- When an agent's `env` setting already sets a proxy variable, nothing is injected.

## SSH remotes

The allowlist on your machine is the only list. It is sent to the remote server when you
connect and again whenever it changes. The remote server runs its own loopback proxy for
ACP agents and MCP servers that run on the remote (not on Windows remote hosts).

With `remote.relay_http_through_client` (on by default), the remote host's downloads go
through your machine. A remote without internet access can then still fetch language
servers.

## The agent sandbox

The network a sandboxed command can reach is the sandbox's own grant limited by the
allowlist (plus loopback). The sandbox can narrow the allowlist but not widen it. This also
holds on WSL and on SSH remotes. Changes take effect from the next command.

## Built-in browser

The built-in browser panel is operated by you, like a terminal, and is not restricted by
default. Turn on `browser.use_egress_proxy` (**Apply the Allowlist to the Built-in
Browser**) to route it through the allowlist; this takes effect the next time the browser
starts.

## Telemetry, collaboration and updates

- This build sends neither telemetry nor crash reports to Zed, and does not show them in
  the settings.
- Collaboration (calls and screen sharing) is not built into ForgeKairos.
- `auto_install_extensions` is empty, so nothing is downloaded at startup.
- `auto_update` is off. The update check asks only ForgeKairos's GitHub Releases (see
  [Installation and updates](install.md)).
- Edit prediction is off (`edit_predictions.provider` is `"none"`).
