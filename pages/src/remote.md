# Remote development

[日本語](ja/remote.md)

For connecting to SSH hosts, WSL and containers, see
[Zed's remote development documentation](https://zed.dev/docs/remote-development). This page
describes what ForgeKairos changes.

## The bundled remote server

ForgeKairos ships a statically linked (musl) remote server for Linux x86_64 and uploads it
when you connect, without asking. Because it does not depend on the host's glibc, it works on
Linux hosts too old for VS Code's remote server; it has been tested with Ubuntu 18.04.
WSL and Docker connections use the same server.

The remote server's files are named `forgekairos-remote-server-*`. When it cleans up old
files on the host, it deletes only ForgeKairos's own files, never those of the official
Zed.

## One server process per host

Connections from one ForgeKairos to the same host share one server process, and each project
is a session inside it. A session ends 10 minutes after its last connection; the process
exits 60 seconds after its last session. When it starts, it removes its own leftovers: old
state of sessions that are gone, server logs beyond the newest 5, and language server
versions not used for 30 days.

## Reconnecting

After a network drop or sleep, ForgeKairos reconnects automatically: it retries up to 15
times over about three minutes, and checks the connection as soon as the computer wakes up.
A dead connection is detected within about 45 seconds.

Connecting takes fewer round trips: the host's environment is examined in one SSH command and
the results are remembered.

## Terminals that survive disconnects

On SSH hosts other than Windows, the remote server keeps the shell of each terminal alive
(`terminal.remote_persistence.enabled`, on by default). Commands keep running when the
connection drops or ForgeKairos is restarted. When you reconnect, the recent output (up to
4 MiB) is shown again and the terminal continues. The shell ends when you close the tab, or
after `terminal.remote_persistence.detached_timeout_hours` (24) without a terminal attached.

Tasks, agent shells, WSL, Docker and Windows hosts do not use this.

## Hosts without internet access

With `remote.relay_http_through_client` (on by default), the host's downloads (language
servers, node, extensions, agent binaries) go through your machine. Language servers can
then be installed on a host that cannot reach the internet. The setting applies without
reconnecting.

When a language server or formatter is missing, a notification offers to install it
(`dev_tools.suggest_missing`). While connected to a remote host, it is installed on that host.

## File watching on Linux

On Linux (locally and on remote hosts), ForgeKairos adds inotify watches only while all of
the user's processes together use less than a quarter of the watch limit, and watches the
remaining folders by polling. Builds and other tools on the host then do not run out of
watches. Changes in polled folders appear a few seconds later.

## Old git on the host

Git integration works with old versions of git on the host. See [Git](git.md#old-git).

## Git status of untracked files

In very large repositories, `git.status_untracked_files` set to `"normal"` or `"no"` makes git
status cheaper. Over SSH, the same value is used on the host.

## File transfer

In an SSH project, the terminal's file transfer can use the remote host as its local side;
the transfer commands then run on the host. See [Terminal macros and sessions](terminal-macros.md#file-transfer).
