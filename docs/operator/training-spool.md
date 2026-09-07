# The training spool

**Nav: Spool.** `/dashboard/admin/training-spool`

A recorder. With it on, every exchange between this server and one of its
models — slot, model, the full prompt, the full completion — is written as one
JSON line per call into per-day files under `<workdir>/training-spool`.

What it is for: a strong model answering on a slot leaves a trace of how that
slot should be answered, and those traces are the raw material for training a
small local model to take the slot over. The recording is what this page does;
the training is yours to do elsewhere, with the files.

**The spool holds raw prompts, including the recalled memory of every person
this deployment serves.** It never leaves the machine, but treat the directory
exactly like the memory itself: back it up deliberately or prune it, and clear
it before sharing a dataset.

The single checkbox turns it on and off; **On disk** below lists what has
accumulated. Health probes are never recorded, and a failed call has no
completion to record.
