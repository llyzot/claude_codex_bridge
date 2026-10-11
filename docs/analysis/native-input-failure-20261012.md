# Immediate provider input failure observation

The three owned protocol adapters previously had separate writer threads that
exited after `write_all`/`flush` failed. Pi/Codex could discover the disconnected
sender on a later enqueue, but the first failed input could leave the active
job without a fault. The new first-write regression reproduced that absence.

The adapters now share `provider_input::spawn`: a bounded 16-message channel,
UTF-8 JSONL serialization and a writer-failure flag published with release/acquire
ordering. Failed serialization, write or flush sets the flag immediately; no
second enqueue is required. Normal sender closure drains already queued input
and delivers intentional stdin EOF without setting the failure flag.

Pi/Codex fold this observation into their existing input-closed state. Claude
also consults it for readiness, transport observation and polling. The normal
poll path sends it into the durable transport-fault handling added previously.
Already validated results retain priority. A failed write may have delivered
some or all bytes, so the active job is not labelled definitely unsent, completed
or safe to replay. Explicit recovery still requires the existing fault identity
and execution/process exit evidence. Claude's intended idle shutdown remains a
separate drain/flush path.

The provider regressions close stdin after protocol initialization while leaving
the output pipe and process alive. A single admissible request is then enough
to publish `transport_closed: stdin`. All three provider cases verify that the
active task stays fenced, no second launch occurs, new submissions are blocked,
and a deliberately submitted new task works after verified exit and recovery.
The fixtures use the base interpreter to avoid extra Windows redirector handles.

Unit tests exercise short writes across UTF-8/JSONL frames, orderly sender
closure, immediate write failure and a flush failure after bytes have already
been accepted. This is notification of an actual I/O failure, not a claim to
detect a slow reader, a full-but-open pipe, every silent hang or successful
provider consumption. No payload or raw I/O error is copied into fault records.

Validation counts and binary hashes are in the adjacent evidence file. This
checkpoint uses isolated controlled providers; it does not change production
installation, provider accounts or the default runtime.
