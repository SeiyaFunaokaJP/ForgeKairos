# AI and network restrictions at a glance

[日本語](ja/overview.md)

These diagrams show how ForgeKairos restricts AI and outbound network traffic. For the detailed rules, see
[Outbound network control](network.md) and [AI](ai.md).

## Is a request let through?

An HTTP request started by an AI feature is checked in this order.

```mermaid
flowchart TD
    R["Outbound request"] --> O{"Marked as the editor's own traffic?<br/>(language servers, extensions, updates, Git, ...)"}
    O -- "Yes" --> FREE["Not restricted"]
    O -- "No (treated as AI traffic)" --> L{"Loopback?<br/>127.0.0.0/8, ::1, localhost"}
    L -- "Yes" --> ALLOW["Allowed"]
    L -- "No" --> B{"Built-in allowance?<br/>(marketplace, ACP registry,<br/>ForgeKairos update check; fixed paths only)"}
    B -- "Yes" --> ALLOW
    B -- "No" --> A{"Host matches the allowlist?<br/>network_allowlist.json"}
    A -- "Yes" --> ALLOW
    A -- "No" --> DENY["Denied"]
    ALLOW --> RD{"Redirect?"}
    RD -- "No" --> DONE["Sent"]
    RD -- "Yes: check the next hop the same way<br/>(credential headers dropped on a host change)" --> L
```

- Loopback is let through even with an empty allowlist, so local LLMs (Ollama, LM Studio, llama.cpp) work
  without any setup.
- Allowing a host allows every port on it.

## Who is restricted, and through which path

```mermaid
flowchart LR
    subgraph APP["ForgeKairos"]
        AI["AI features<br/>providers, edit predictions,<br/>fetch and web search, MCP (HTTP)"]
        TOOLS["Editor tools<br/>language servers, extensions,<br/>updates, Git, remote server"]
        GATE["Allowlist check<br/>(loopback + built-in allowances + allowed_hosts)"]
        PROXY["Loopback proxy<br/>(loopback + allowed_hosts only;<br/>no built-in allowances)"]
    end
    subgraph CHILD["Child processes"]
        ACP["ACP agents"]
        MCP["MCP servers (stdio)"]
        NPM["npm installing agents"]
    end
    SB["Agent sandbox<br/>(WSL / bubblewrap / Seatbelt)"]
    BR["Built-in browser"]
    NET(("Internet"))

    AI --> GATE --> NET
    TOOLS -- "not restricted" --> NET
    ACP & MCP & NPM -- "HTTP_PROXY / HTTPS_PROXY" --> PROXY --> NET
    SB -- "sandbox's own allowances ∩ allowlist" --> NET
    BR -- "not restricted by default;<br/>browser.use_egress_proxy routes it through the allowlist" --> NET
```

- The child-process proxy relies on the program reading the proxy environment variables. A program that
  ignores them cannot be restricted. Turn it off with `proxy_child_processes: false`.
- The sandbox can narrow the allowlist but never widen it.

## Before a cloud AI provider is used

By default no AI runs, and nothing is sent to an external provider.

```mermaid
sequenceDiagram
    actor U as User
    participant FK as ForgeKairos
    participant AL as Allowlist
    participant P as Cloud provider

    Note over FK: agent.enabled = false (default). All AI is off
    U->>FK: Settings → AI → Enable the agent
    Note over FK: Local LLMs (loopback) work from here
    U->>FK: Choose an external provider
    FK-->>U: Warning (input and conversations are sent) and required hosts
    U->>FK: Tick the acknowledgement
    Note over FK: Recorded in acknowledged_external_ai_providers
    U->>AL: Add to allowlist
    FK->>P: Requests get through only now
    Note over U,AL: Revoking asks whether to remove hosts<br/>that no other provider uses
```

Acknowledging without adding the hosts leaves traffic to that provider denied.

## When the agent runs a command or tool

```mermaid
flowchart TD
    T["Agent tool call"] --> AC{"Always confirmed in every mode?<br/>safety rules, always_confirm,<br/>outside the sandbox, permission escalation,<br/>writes to .zed or settings, outside the project"}
    AC -- "Yes" --> ASK["Ask the user"]
    AC -- "No" --> M{"agent.tool_permissions.default"}
    M -- "confirm (default)" --> ASK
    M -- "deny" --> NO["Not run"]
    M -- "allow" --> RUN
    M -- "auto" --> J{"Ask the judge model<br/>(10-second limit)"}
    J -- "SAFE" --> RUN
    J -- "anything else" --> ASK
    ASK -- "Approved" --> RUN["Run inside the sandbox"]
    RUN --> NETSB["Network limited by the allowlist<br/>credentials (~/.ssh, .npmrc, ...) unreadable"]
    RUN --> PAUSE["After a write to the project,<br/>that folder's own code (formatters,<br/>language server build scripts, ...)<br/>is paused until resumed"]
```

- `auto` sends the command and paths to the judge model's provider, so a local model is recommended.
- The only way to turn the sandbox off is `agent.sandbox_permissions.allow_unsandboxed`, and while it is
  off every command is confirmed.

## SSH remotes

```mermaid
flowchart LR
    subgraph LOCAL["Local machine"]
        AL["Allowlist<br/>(the local one only)"]
        CL["ForgeKairos"]
    end
    subgraph REMOTE["Remote host"]
        RS["Remote server"]
        RP["Remote loopback proxy<br/>(not on Windows remotes)"]
        RA["ACP agents / MCP servers"]
    end
    NET(("Internet"))

    AL --> CL
    CL -- "sent on connect and on every change" --> RS
    RS --> RP
    RA -- "HTTP_PROXY" --> RP --> NET
    RS -- "downloads<br/>(relay_http_through_client, on by default)" --> CL --> NET
```

A remote with no internet access can still fetch language servers through the local machine.
