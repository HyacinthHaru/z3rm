---
title: Local and remote sessions
description: The same mux protocol over local sockets and SSH-forwarded channels.
translationKey: concept-remote
section: concepts
order: 3
status: verified
---

# Local and remote sessions

Local and remote clients use the same framed binary mux protocol. Local transport uses a Unix socket or named pipe; remote transport runs through an SSH-forwarded channel.

## Attach remotely

```sh
z3rm attach --ssh ssh://user@example.com/path/to/project
```

The remote host owns its PTYs, grid state, files, and shadow snapshots. The client owns presentation and input events. A reconnect always reconciles from a server snapshot.
