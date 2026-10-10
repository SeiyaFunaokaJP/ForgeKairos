# AI と通信の制限の全体図

[English](../overview.md)

ForgeKairos の AI と外部通信の制限を図で示す。細かい規則は [外部通信の統制](network.md) と [AI](ai.md) を見る。

## 1 つの要求が通るか

AI の機能が始める HTTP の要求は、次の順に判定する。

```mermaid
flowchart TD
    R["外部への要求"] --> O{"エディター自身の通信の印があるか<br/>(言語サーバー、拡張機能、更新、Git など)"}
    O -- "ある" --> FREE["制限しない"]
    O -- "無い (AI の通信として扱う)" --> L{"loopback か<br/>127.0.0.0/8、::1、localhost"}
    L -- "はい" --> ALLOW["許可"]
    L -- "いいえ" --> B{"組み込みの許可か<br/>(マーケットプレイス、ACP レジストリ、<br/>ForgeKairos の更新の確認。決まったパスだけ)"}
    B -- "はい" --> ALLOW
    B -- "いいえ" --> A{"許可リストのホストに一致するか<br/>network_allowlist.json"}
    A -- "はい" --> ALLOW
    A -- "いいえ" --> DENY["拒否"]
    ALLOW --> RD{"リダイレクトか"}
    RD -- "いいえ" --> DONE["送る"]
    RD -- "はい: 次の hop も同じ判定へ<br/>(別のホストなら資格情報のヘッダーを外す)" --> L
```

- 許可リストが空でも loopback は通るので、ローカル LLM (Ollama、LM Studio、llama.cpp) は設定なしで使える。
- ホストを許可すると、そのホストのすべてのポートを許可する。

## 誰がどの経路で制限されるか

```mermaid
flowchart LR
    subgraph APP["ForgeKairos 本体"]
        AI["AI の機能<br/>プロバイダー、編集予測、<br/>fetch と Web 検索、MCP (HTTP)"]
        TOOLS["エディターの道具<br/>言語サーバー、拡張機能、<br/>更新、Git、リモートのサーバー"]
        GATE["許可リストの判定<br/>(loopback + 組み込みの許可 + allowed_hosts)"]
        PROXY["loopback のプロキシ<br/>(loopback + allowed_hosts だけ。<br/>組み込みの許可は使わない)"]
    end
    subgraph CHILD["子プロセス"]
        ACP["ACP エージェント"]
        MCP["MCP サーバー (stdio)"]
        NPM["エージェントを入れる npm"]
    end
    SB["エージェントのサンドボックス<br/>(WSL / bubblewrap / Seatbelt)"]
    BR["内蔵ブラウザー"]
    NET(("インターネット"))

    AI --> GATE --> NET
    TOOLS -- "制限しない" --> NET
    ACP & MCP & NPM -- "HTTP_PROXY / HTTPS_PROXY" --> PROXY --> NET
    SB -- "サンドボックス自身の許可 ∩ 許可リスト" --> NET
    BR -- "既定は制限しない。<br/>browser.use_egress_proxy で許可リストを通す" --> NET
```

- 子プロセスのプロキシは、プログラムがプロキシの環境変数を読むことに頼る。読まないプログラムは制限できない。
  `proxy_child_processes: false` で切れる。
- サンドボックスは許可リストを狭めることはできるが、広げることはできない。

## クラウドの AI を使い始めるまで

既定ではどの AI も動かず、外部のプロバイダーには何も送らない。

```mermaid
sequenceDiagram
    actor U as ユーザー
    participant FK as ForgeKairos
    participant AL as 許可リスト
    participant P as クラウドのプロバイダー

    Note over FK: agent.enabled = false (既定)。AI はすべてオフ
    U->>FK: 設定 → AI → エージェントを有効にする
    Note over FK: ローカル LLM (loopback) はこの時点で使える
    U->>FK: 外部のプロバイダーを選ぶ
    FK-->>U: 警告 (入力と会話が送られる) と必要なホスト
    U->>FK: 確認のチェック
    Note over FK: acknowledged_external_ai_providers に記録
    U->>AL: 許可リストに追加
    FK->>P: ここで初めて要求が通る
    Note over U,AL: 承諾を取り消すと、ほかが使わないホストを<br/>許可リストから外すか尋ねる
```

承諾しても許可リストに足さなければ、そのプロバイダーへの通信は拒否のまま。

## エージェントがコマンドやツールを実行するとき

```mermaid
flowchart TD
    T["エージェントのツールの呼び出し"] --> AC{"どのモードでも確認するものか<br/>安全の規則、always_confirm、<br/>サンドボックスの外、権限の昇格、<br/>.zed や設定への書き込み、プロジェクトの外"}
    AC -- "はい" --> ASK["ユーザーに確認"]
    AC -- "いいえ" --> M{"agent.tool_permissions.default"}
    M -- "confirm (既定)" --> ASK
    M -- "deny" --> NO["実行しない"]
    M -- "allow" --> RUN
    M -- "auto" --> J{"判定用のモデルに尋ねる<br/>(10 秒の上限)"}
    J -- "SAFE" --> RUN
    J -- "それ以外" --> ASK
    ASK -- "承認" --> RUN["サンドボックスの中で実行"]
    RUN --> NETSB["ネットワークは許可リストで制限<br/>資格情報 (~/.ssh、.npmrc など) は読めない"]
    RUN --> PAUSE["プロジェクトに書き込んだら、<br/>そのフォルダーのコード (フォーマッター、<br/>言語サーバーのビルドスクリプトなど) を<br/>再開するまで止める"]
```

- `auto` はコマンドとパスを判定用のモデルのプロバイダーに送るので、ローカルのモデルを勧める。
- サンドボックスを切れるのは `agent.sandbox_permissions.allow_unsandboxed` だけで、切っている間はすべてのコマンドで確認する。

## SSH のリモート

```mermaid
flowchart LR
    subgraph LOCAL["手元の PC"]
        AL["許可リスト<br/>(手元のものだけ)"]
        CL["ForgeKairos"]
    end
    subgraph REMOTE["リモートのホスト"]
        RS["リモートのサーバー"]
        RP["リモートの loopback のプロキシ<br/>(Windows のリモートを除く)"]
        RA["ACP エージェント / MCP サーバー"]
    end
    NET(("インターネット"))

    AL --> CL
    CL -- "接続時と変更のたびに送る" --> RS
    RS --> RP
    RA -- "HTTP_PROXY" --> RP --> NET
    RS -- "ダウンロード<br/>(relay_http_through_client、既定でオン)" --> CL --> NET
```

インターネットに出られないリモートでも、手元の PC 経由で言語サーバーを取ってこられる。
