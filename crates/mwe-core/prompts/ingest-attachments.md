---
name: ingest-attachments
description: The `ingest` classifier's rules for a turn that carries media — describe each item inside a fact and claim its catalog_id, never write the embed marker by hand. Appended to the turn context by `ingest::build_prompt`, and ONLY when the turn has attachments.
version: 1.0
default_version_at_bootstrap: v1.0
part_of: ingest
appended_when: the turn carries at least one attachment
---

# Prompt: ingest-attachments

A **part** of the `ingest` classifier, not a slot of its own: the rules for
claiming and describing the media riding a turn, sent only on a turn that has
any.

## Why it is a part

Roughly one turn in five carries media on a real week, and `ingest.md` is read
in full on every turn by the smallest model in the fleet. The same reasoning
as `ingest-assistant-turn`, and the same placement: it rides the TURN, so the
system half stays byte-identical and the prompt cache keeps serving it.

## Runtime contract

- **Call site**: `crate::ingest::build_prompt`, in the turn context, when the
  request carries at least one attachment.
- **Placeholders**: none.
- **Output**: none of its own — it fills the `attachments` array of the
  extractions the `ingest` prompt defines.

## Prompt

```text
This turn carries an `attachments:` section: the user sent media (photos, videos, audio, documents) alongside the message. Each entry shows its `catalog_id`, `kind`, and — when available — a `caption` and/or a consumer-supplied `description`. For `kind: photo` WITHOUT a description, the image itself rides this call: LOOK at it.

Your job per attachment:

- **Describe it inside a fact.** For a photo, fuse what you SEE with the user's caption into one extraction's `body` — concrete, third person, the things worth remembering (who, what, where, occasion): "Photo of Frodo and Sam at the garden gate, spring." For `video`, the caption is the only material (no video understanding) — record it as the fact. For `audio`, the host usually already transcribed it (the transcript IS the message text); the attachment is the recording itself. For `doc`, describe from caption/description.
- **Claim it**: put the attachment's `catalog_id` (copied EXACTLY from the `attachments:` section — never an id you were not shown) in the describing extraction's `attachments` array. One extraction can claim several media (an album described together); media you do not claim are filed by the engine only when they carry a caption or description (a text-less unclaimed item stays catalogued but enters no page) — claimed and described is always better.
- **An attachment the turn's text already carries** — an audio note the host transcribed (the transcript IS the message), a document whose content the message restates — needs no fact of its own: claim it on the extraction that records what it says, so the recording rides as provenance. When the turn produces no extraction that can carry it (the transcript became a behaviour rule, or the turn is a `skip`), leave it unclaimed — never emit a contentless extraction (a bare "audio"/"foto" body) just to hold a media item.
- **Never write marker syntax** (`{{embed=…}}`) in any `body` — the engine renders the markers from your `attachments` claims.
- When a consumer-supplied `description` is present, trust it as what the media shows (you will not see the bytes) and still fuse it with the caption into the fact.
- Attachments bias the intent toward `capture`: a photo with no text is still a capture turn (describe the photo). A recall question that merely mentions an old photo claims nothing.
```
